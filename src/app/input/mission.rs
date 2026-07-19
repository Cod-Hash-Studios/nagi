use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::state::{
    AppState, AttentionAnswerDraft, MissionHandoffDraft, MissionHandoffLaunchRequest, Mode,
    NewMissionDraft, NewMissionLaunchRequest, NewMissionStep,
};

pub(crate) fn open_new_mission(
    state: &mut AppState,
    terminal_runtimes: &crate::terminal::TerminalRuntimeRegistry,
) {
    let cwd = state
        .active
        .and_then(|index| state.workspaces.get(index))
        .and_then(|workspace| {
            workspace.resolved_identity_cwd_from(&state.terminals, terminal_runtimes)
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let repository = crate::workspace::git_worktree_info(&cwd)
        .map(|info| info.repo_root)
        .and_then(|path| std::fs::canonicalize(path).ok());
    let (repository_path, mut error) = repository.map_or_else(
        || {
            (
                cwd,
                Some("Open Nagi inside a Git checkout to create a mission".to_owned()),
            )
        },
        |path| (path, None),
    );
    let recipe = crate::project_recipe::detect(&repository_path);
    let project_recipe_summary = match crate::project_recipe::load_contract(&repository_path) {
        Ok(Some(contract)) => Some(project_recipe_summary(&contract)),
        Ok(None) => None,
        Err(contract_error) => {
            error = Some(format!("Invalid .nagi/project.toml: {contract_error}"));
            None
        }
    };
    state.new_mission = Some(NewMissionDraft {
        step: NewMissionStep::Objective,
        repository_path,
        proof_command: recipe.command_line.clone(),
        recipe,
        project_recipe_summary,
        objective: String::new(),
        criteria: String::new(),
        provider_index: 0,
        workspace_write_confirmed: false,
        error,
    });
    state.mission_action_error = None;
    state.mode = Mode::NewMission;
}

pub(crate) fn handle_new_mission_key(state: &mut AppState, key: KeyEvent) {
    let Some(step) = state.new_mission.as_ref().map(|draft| draft.step) else {
        state.mode = Mode::Navigator;
        return;
    };
    if key.code == KeyCode::Esc {
        state.new_mission = None;
        state.mode = Mode::Navigator;
        return;
    }
    if key.code == KeyCode::BackTab {
        let draft = state.new_mission.as_mut().unwrap();
        draft.step = previous_new_mission_step(step);
        draft.error = None;
        return;
    }

    match step {
        NewMissionStep::Objective | NewMissionStep::Criteria | NewMissionStep::ProofCommand => {
            match key.code {
                KeyCode::Backspace => {
                    active_new_mission_text(state).pop();
                    state.new_mission.as_mut().unwrap().error = None;
                }
                KeyCode::Enter => advance_new_mission(state),
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    insert_new_mission_text(state, &character.to_string());
                }
                _ => {}
            }
        }
        NewMissionStep::Provider => match key.code {
            KeyCode::Left | KeyCode::Up | KeyCode::Char('h' | 'k') => {
                let draft = state.new_mission.as_mut().unwrap();
                draft.provider_index = draft.provider_index.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Char('l' | 'j') => {
                let draft = state.new_mission.as_mut().unwrap();
                draft.provider_index = (draft.provider_index + 1).min(3);
            }
            KeyCode::Enter => advance_new_mission(state),
            _ => {}
        },
        NewMissionStep::Confirm => match key.code {
            KeyCode::Char(' ') | KeyCode::Char('w') => {
                let draft = state.new_mission.as_mut().unwrap();
                draft.workspace_write_confirmed = !draft.workspace_write_confirmed;
                draft.error = None;
            }
            KeyCode::Enter => submit_new_mission(state),
            _ => {}
        },
    }
}

pub(crate) fn insert_new_mission_text(state: &mut AppState, text: &str) -> bool {
    let Some(draft) = state.new_mission.as_mut() else {
        return false;
    };
    let (field, max): (&mut String, usize) = match draft.step {
        NewMissionStep::Objective => (&mut draft.objective, 8_192),
        NewMissionStep::Criteria => (&mut draft.criteria, 16_384),
        NewMissionStep::ProofCommand => (&mut draft.proof_command, 4_096),
        NewMissionStep::Provider | NewMissionStep::Confirm => return false,
    };
    let remaining = max.saturating_sub(field.len());
    field.extend(
        text.chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .take(remaining),
    );
    draft.error = None;
    true
}

fn active_new_mission_text(state: &mut AppState) -> &mut String {
    let draft = state.new_mission.as_mut().unwrap();
    match draft.step {
        NewMissionStep::Objective => &mut draft.objective,
        NewMissionStep::Criteria => &mut draft.criteria,
        NewMissionStep::ProofCommand => &mut draft.proof_command,
        NewMissionStep::Provider | NewMissionStep::Confirm => unreachable!(),
    }
}

fn advance_new_mission(state: &mut AppState) {
    let draft = state.new_mission.as_mut().unwrap();
    let validation = match draft.step {
        NewMissionStep::Objective => (!draft.objective.trim().is_empty())
            .then_some(())
            .ok_or("Describe the outcome you want"),
        NewMissionStep::Criteria => parse_acceptance_criteria(&draft.criteria).map(|_| ()),
        NewMissionStep::ProofCommand => {
            crate::project_recipe::parse_command_line(&draft.proof_command).map(|_| ())
        }
        NewMissionStep::Provider | NewMissionStep::Confirm => Ok(()),
    };
    match validation {
        Ok(()) => {
            draft.step = next_new_mission_step(draft.step);
            draft.error = None;
        }
        Err(error) => draft.error = Some(error.to_owned()),
    }
}

fn submit_new_mission(state: &mut AppState) {
    if state.detach_exits {
        if let Some(draft) = state.new_mission.as_mut() {
            draft.error = Some("Missions require persistent session mode".into());
        }
        return;
    }
    let draft = state.new_mission.as_mut().unwrap();
    if !draft.workspace_write_confirmed {
        draft.error = Some("Press space to confirm the provider write scope".into());
        return;
    }
    let criteria = match parse_acceptance_criteria(&draft.criteria) {
        Ok(criteria) => criteria,
        Err(error) => {
            draft.error = Some(error.into());
            return;
        }
    };
    let (program, args) = match crate::project_recipe::parse_command_line(&draft.proof_command) {
        Ok(command) => command,
        Err(error) => {
            draft.error = Some(error.into());
            return;
        }
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64);
    let slug = mission_slug(&draft.objective);
    let mission_id = format!("{slug}-{:x}", stamp & 0xff_ffff);
    let run_id = format!("run-{mission_id}");
    let title = draft.objective.trim().chars().take(96).collect::<String>();
    let repository_path = draft.repository_path.to_string_lossy().into_owned();
    let provider = match draft.provider_index {
        1 => crate::api::schema::MissionProvider::ClaudeCode,
        2 => crate::api::schema::MissionProvider::OpenCode,
        3 => crate::api::schema::MissionProvider::Acp,
        _ => crate::api::schema::MissionProvider::Codex,
    };
    state.request_new_mission = Some(NewMissionLaunchRequest {
        create: crate::api::schema::MissionCreateParams {
            mission_id: mission_id.clone(),
            title,
            repository_path,
            objective: draft.objective.trim().to_owned(),
            acceptance_criteria: criteria.clone(),
        },
        configure: crate::api::schema::MissionConfigureParams {
            mission_id: mission_id.clone(),
            checks: vec![crate::api::schema::MissionCheck::Command {
                id: format!("{}-proof", draft.recipe.id),
                program,
                args,
                cwd: ".".into(),
                relevant_paths: vec![crate::api::schema::MissionPathRule::All],
                required_artifacts: Vec::new(),
                include_ignored: false,
                required: true,
                covers: (0..criteria.len()).collect(),
            }],
        },
        start: crate::api::schema::MissionStartParams {
            mission_id: mission_id.clone(),
            run_id,
            provider,
            mode: crate::api::schema::MissionProviderMode::Managed,
            worktree_path: None,
            execute_declared_checks: true,
            execute_project_recipe: draft.project_recipe_summary.is_some(),
        },
        workspace_write_confirmed: true,
        branch: format!("mission/{mission_id}"),
    });
    state.selected_mission_id = Some(mission_id);
    state.new_mission = None;
    state.mode = Mode::Navigator;
}

fn project_recipe_summary(contract: &crate::project_recipe::ProjectContract) -> String {
    let mut parts = Vec::new();
    if contract.setup.is_some() {
        parts.push("setup".to_owned());
    }
    if !contract.services.is_empty() {
        parts.push(format!("{} service(s)", contract.services.len()));
    }
    if !contract.cleanup.is_empty() {
        parts.push(format!("{} cleanup command(s)", contract.cleanup.len()));
    }
    if !contract.worktree.copy_ignored.is_empty() {
        parts.push(format!(
            "{} explicit file copy rule(s)",
            contract.worktree.copy_ignored.len()
        ));
    }
    if parts.is_empty() {
        "validated project contract".to_owned()
    } else {
        parts.join(" · ")
    }
}

fn parse_acceptance_criteria(input: &str) -> Result<Vec<String>, &'static str> {
    let criteria = input
        .split(['\n', ';'])
        .map(str::trim)
        .filter(|criterion| !criterion.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if criteria.is_empty() {
        return Err("Add at least one acceptance criterion");
    }
    if criteria.len() > 16 {
        return Err("A mission supports at most 16 acceptance criteria");
    }
    if criteria.iter().any(|criterion| criterion.len() > 1_024) {
        return Err("Each acceptance criterion must fit within 1024 bytes");
    }
    Ok(criteria)
}

fn mission_slug(objective: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in objective.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character);
            separator = false;
        } else {
            separator = true;
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-').to_owned();
    if slug.is_empty() {
        "mission".to_owned()
    } else {
        slug
    }
}

const fn next_new_mission_step(step: NewMissionStep) -> NewMissionStep {
    match step {
        NewMissionStep::Objective => NewMissionStep::Criteria,
        NewMissionStep::Criteria => NewMissionStep::ProofCommand,
        NewMissionStep::ProofCommand => NewMissionStep::Provider,
        NewMissionStep::Provider | NewMissionStep::Confirm => NewMissionStep::Confirm,
    }
}

const fn previous_new_mission_step(step: NewMissionStep) -> NewMissionStep {
    match step {
        NewMissionStep::Objective | NewMissionStep::Criteria => NewMissionStep::Objective,
        NewMissionStep::ProofCommand => NewMissionStep::Criteria,
        NewMissionStep::Provider => NewMissionStep::ProofCommand,
        NewMissionStep::Confirm => NewMissionStep::Provider,
    }
}

pub(crate) fn handle_mission_inspector_key(state: &mut AppState, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => state.mode = Mode::Navigator,
        KeyCode::Char('h') => open_mission_handoff(state),
        KeyCode::Tab | KeyCode::Right => change_mission_inspector_tab(state, 1),
        KeyCode::BackTab | KeyCode::Left => change_mission_inspector_tab(state, -1),
        KeyCode::Char('r') if state.mission_inspector_tab > 0 => {
            queue_plugin_inspector_refresh(state)
        }
        KeyCode::Enter if state.mission_inspector_tab > 0 => queue_plugin_inspector_refresh(state),
        KeyCode::Char('p') | KeyCode::Enter => {
            state.proof_review_selected = 0;
            state.proof_review_scroll = 0;
            state.mode = Mode::ProofReview;
        }
        KeyCode::Char('a') => state.open_attention_inbox(),
        KeyCode::Up | KeyCode::Char('k') => {
            state.mission_inspector_scroll = state.mission_inspector_scroll.saturating_sub(1)
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let body_height = state.navigator_inner_rect().height.saturating_sub(4);
            let max = crate::ui::mission_inspector_max_scroll(state, body_height);
            state.mission_inspector_scroll =
                state.mission_inspector_scroll.saturating_add(1).min(max);
        }
        KeyCode::PageUp => {
            state.mission_inspector_scroll = state.mission_inspector_scroll.saturating_sub(8)
        }
        KeyCode::PageDown => {
            let body_height = state.navigator_inner_rect().height.saturating_sub(4);
            let max = crate::ui::mission_inspector_max_scroll(state, body_height);
            state.mission_inspector_scroll =
                state.mission_inspector_scroll.saturating_add(8).min(max);
        }
        KeyCode::Home => state.mission_inspector_scroll = 0,
        KeyCode::End => {
            let body_height = state.navigator_inner_rect().height.saturating_sub(4);
            state.mission_inspector_scroll =
                crate::ui::mission_inspector_max_scroll(state, body_height);
        }
        _ => {}
    }
}

fn change_mission_inspector_tab(state: &mut AppState, direction: i8) {
    let tab_count = state
        .mission_plugin_inspector_tabs()
        .len()
        .saturating_add(1);
    if tab_count <= 1 {
        state.mission_inspector_tab = 0;
        return;
    }
    state.mission_inspector_tab = if direction < 0 {
        state
            .mission_inspector_tab
            .checked_sub(1)
            .unwrap_or(tab_count - 1)
    } else {
        (state.mission_inspector_tab + 1) % tab_count
    };
    state.mission_inspector_scroll = 0;
    if state.mission_inspector_tab == 0 {
        state.request_plugin_inspector_refresh = None;
        state.plugin_inspector_active_key = None;
        state.plugin_inspector_log_id = None;
        state.plugin_inspector_document = None;
        state.plugin_inspector_error = None;
    } else {
        queue_plugin_inspector_refresh(state);
    }
}

fn queue_plugin_inspector_refresh(state: &mut AppState) {
    let tabs = state.mission_plugin_inspector_tabs();
    let Some(tab) = state
        .mission_inspector_tab
        .checked_sub(1)
        .and_then(|index| tabs.get(index))
        .cloned()
    else {
        state.mission_inspector_tab = 0;
        return;
    };
    let Some(mission_id) = state.selected_mission_id.clone() else {
        state.plugin_inspector_error = Some("Mission no longer exists".to_owned());
        return;
    };
    state.plugin_inspector_active_key = Some(tab.key());
    state.plugin_inspector_document = None;
    state.plugin_inspector_error = None;
    state.request_plugin_inspector_refresh =
        Some(crate::app::state::PluginInspectorRefreshRequest {
            mission_id,
            plugin_id: tab.plugin_id,
            tab_id: tab.tab_id,
            source: tab.source,
        });
}

fn open_mission_handoff(state: &mut AppState) {
    let Some(mission) = state.selected_mission_id.as_ref().and_then(|selected| {
        state
            .mission_views
            .iter()
            .find(|mission| mission.mission_id == *selected)
    }) else {
        state.mission_action_error = Some("Select a mission before handing it off".into());
        return;
    };
    if !matches!(
        mission.status,
        crate::api::schema::MissionStatus::Blocked
            | crate::api::schema::MissionStatus::Failed
            | crate::api::schema::MissionStatus::ReviewRequired
            | crate::api::schema::MissionStatus::ReadyToClose
    ) {
        state.mission_action_error =
            Some("Handoff is available after a run blocks, fails, or enters review".into());
        return;
    }
    if mission.unresolved_attention_count != 0 {
        state.mission_action_error = Some("Resolve every attention request before handoff".into());
        return;
    }
    let Some(source_provider) = mission.run.as_ref().map(|run| run.provider) else {
        state.mission_action_error = Some("Mission has no run to hand off".into());
        return;
    };
    let target_provider = next_handoff_provider(source_provider, source_provider, 1);
    let mission_id = mission.mission_id.clone();
    state.mission_handoff = Some(MissionHandoffDraft {
        mission_id: mission_id.clone(),
        source_provider,
        target_provider,
        artifact: None,
        workspace_write_confirmed: false,
        loading: true,
        error: None,
    });
    state.request_mission_handoff_preview = Some(crate::api::schema::MissionHandoffPreviewParams {
        mission_id,
        to: target_provider,
    });
    state.mission_action_error = None;
    state.mode = Mode::MissionHandoff;
}

pub(crate) fn handle_mission_handoff_key(state: &mut AppState, key: KeyEvent) {
    if state.mission_handoff.is_none() {
        state.mode = Mode::MissionInspector;
        return;
    }
    match key.code {
        KeyCode::Esc => {
            state.mission_handoff = None;
            state.request_mission_handoff_preview = None;
            state.request_mission_handoff_start = None;
            state.mode = Mode::MissionInspector;
        }
        KeyCode::Left | KeyCode::Up | KeyCode::Char('k') => {
            change_handoff_provider(state, -1);
        }
        KeyCode::Right | KeyCode::Down | KeyCode::Char('j') => {
            change_handoff_provider(state, 1);
        }
        KeyCode::Char(' ') | KeyCode::Char('w') => {
            let draft = state.mission_handoff.as_mut().unwrap();
            draft.workspace_write_confirmed = !draft.workspace_write_confirmed;
            draft.error = None;
        }
        KeyCode::Enter => submit_mission_handoff(state),
        _ => {}
    }
}

fn change_handoff_provider(state: &mut AppState, direction: i8) {
    let draft = state.mission_handoff.as_mut().unwrap();
    if draft.loading {
        return;
    }
    draft.target_provider =
        next_handoff_provider(draft.target_provider, draft.source_provider, direction);
    draft.artifact = None;
    draft.loading = true;
    draft.error = None;
    state.request_mission_handoff_preview = Some(crate::api::schema::MissionHandoffPreviewParams {
        mission_id: draft.mission_id.clone(),
        to: draft.target_provider,
    });
}

fn next_handoff_provider(
    current: crate::api::schema::MissionProvider,
    source: crate::api::schema::MissionProvider,
    direction: i8,
) -> crate::api::schema::MissionProvider {
    use crate::api::schema::MissionProvider;
    let providers = [
        MissionProvider::Codex,
        MissionProvider::ClaudeCode,
        MissionProvider::OpenCode,
        MissionProvider::Acp,
    ];
    let mut index = providers
        .iter()
        .position(|provider| *provider == current)
        .unwrap_or(0);
    loop {
        index = if direction < 0 {
            index.checked_sub(1).unwrap_or(providers.len() - 1)
        } else {
            (index + 1) % providers.len()
        };
        if providers[index] != source {
            return providers[index];
        }
    }
}

fn submit_mission_handoff(state: &mut AppState) {
    let draft = state.mission_handoff.as_mut().unwrap();
    if draft.loading {
        return;
    }
    if !draft.workspace_write_confirmed {
        draft.error = Some("Press space to confirm the provider write scope".into());
        return;
    }
    let Some(artifact) = draft.artifact.as_ref() else {
        draft.error = Some("Wait for a fresh handoff preview".into());
        return;
    };
    state.request_mission_handoff_start = Some(MissionHandoffLaunchRequest {
        params: crate::api::schema::MissionHandoffStartParams {
            mission_id: draft.mission_id.clone(),
            to: draft.target_provider,
            generated_at_millis: artifact.generated_at_millis,
            artifact_sha256: artifact.artifact_sha256.clone(),
        },
        workspace_write_confirmed: true,
    });
    draft.loading = true;
    draft.error = None;
}

pub(crate) fn handle_proof_review_key(state: &mut AppState, key: KeyEvent) {
    let count = crate::ui::proof_check_count(state);
    match key.code {
        KeyCode::Esc => state.mode = Mode::MissionInspector,
        KeyCode::Char('a') => state.open_attention_inbox(),
        KeyCode::Char('c') => {
            let mission_id = state.selected_mission_id.as_ref().and_then(|selected| {
                state
                    .mission_views
                    .iter()
                    .find(|mission| {
                        mission.mission_id == *selected
                            && mission.status == crate::api::schema::MissionStatus::ReadyToClose
                    })
                    .map(|mission| mission.mission_id.clone())
            });
            if let Some(mission_id) = mission_id {
                state.request_mission_close =
                    Some(crate::api::schema::MissionTarget { mission_id });
                state.mission_action_error = None;
            } else {
                state.mission_action_error =
                    Some("Mission needs a verified ready proof before close".into());
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.proof_review_selected = state.proof_review_selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if count > 0 {
                state.proof_review_selected =
                    state.proof_review_selected.saturating_add(1).min(count - 1);
            }
        }
        KeyCode::Home => state.proof_review_selected = 0,
        KeyCode::End => state.proof_review_selected = count.saturating_sub(1),
        _ => {}
    }
    let viewport = state.navigator_inner_rect().height.saturating_sub(9) as usize;
    if state.proof_review_selected < state.proof_review_scroll {
        state.proof_review_scroll = state.proof_review_selected;
    } else if viewport > 0
        && state.proof_review_selected >= state.proof_review_scroll.saturating_add(viewport)
    {
        state.proof_review_scroll = state
            .proof_review_selected
            .saturating_add(1)
            .saturating_sub(viewport);
    }
}

pub(crate) fn handle_attention_inbox_key(state: &mut AppState, key: KeyEvent) {
    if state.attention_answer_input.is_some() {
        match key.code {
            KeyCode::Esc => state.attention_answer_input = None,
            KeyCode::Backspace => {
                state.attention_answer_input.as_mut().unwrap().input.pop();
            }
            KeyCode::Enter => submit_answer_step(state),
            KeyCode::Char(character)
                if (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
                    && state
                        .attention_answer_input
                        .as_ref()
                        .is_some_and(|answer| answer.input.len() < 4 * 1024) =>
            {
                state
                    .attention_answer_input
                    .as_mut()
                    .unwrap()
                    .input
                    .push(character);
            }
            _ => {}
        }
        return;
    }

    let count = state.attention_items.len();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => state.mode = Mode::MissionInspector,
        KeyCode::Up | KeyCode::Char('k') => {
            state.attention_selected = state.attention_selected.saturating_sub(1)
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if count > 0 {
                state.attention_selected =
                    state.attention_selected.saturating_add(1).min(count - 1);
            }
        }
        KeyCode::Home => state.attention_selected = 0,
        KeyCode::End => state.attention_selected = count.saturating_sub(1),
        KeyCode::Char('y') => queue_selected_response(
            state,
            crate::api::schema::MissionResponseDecision::ApproveOnce,
            BTreeMap::new(),
        ),
        KeyCode::Char('s') => queue_selected_response(
            state,
            crate::api::schema::MissionResponseDecision::ApproveForSession,
            BTreeMap::new(),
        ),
        KeyCode::Char('n') => queue_selected_response(
            state,
            crate::api::schema::MissionResponseDecision::Deny,
            BTreeMap::new(),
        ),
        KeyCode::Char('r') | KeyCode::Enter
            if state
                .attention_items
                .get(state.attention_selected)
                .is_some_and(|item| {
                    item.kind == crate::api::schema::AttentionKindV1::ProviderQuestion
                        && matches!(item.state, crate::api::schema::AttentionStateV1::Open)
                }) =>
        {
            if state.attention_items[state.attention_selected]
                .questions
                .is_empty()
            {
                state.attention_error =
                    Some("This provider did not expose a structured answer form".into());
            } else {
                state.attention_answer_input = Some(AttentionAnswerDraft::new());
                state.attention_error = None;
            }
        }
        _ => {}
    }
    state.selected_attention_id = state
        .attention_items
        .get(state.attention_selected)
        .map(|item| item.attention_id.clone());
}

fn submit_answer_step(state: &mut AppState) {
    let Some(item) = state.attention_items.get(state.attention_selected) else {
        state.attention_answer_input = None;
        return;
    };
    let Some(draft) = state.attention_answer_input.as_ref() else {
        return;
    };
    let Some(question) = item.questions.get(draft.question_index).cloned() else {
        state.attention_answer_input = None;
        state.attention_error = Some("The provider question changed; reopen the form".into());
        return;
    };
    let input = draft.input.trim();
    if input.is_empty() {
        state.attention_error = Some("Answer cannot be empty".into());
        return;
    }
    let values = if question.multiple {
        let mut values = input
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        values
    } else {
        vec![input.to_owned()]
    };
    if values.is_empty() {
        state.attention_error = Some("Select at least one answer".into());
        return;
    }
    if !question.custom_allowed
        && values.iter().any(|answer| {
            !question
                .options
                .iter()
                .any(|option| option.label == answer.as_str())
        })
    {
        state.attention_error = Some("Use one of the provider choices shown below".into());
        return;
    }

    let draft = state.attention_answer_input.as_mut().unwrap();
    draft.answers.insert(question.id, values);
    if draft.question_index + 1 < item.questions.len() {
        draft.question_index += 1;
        draft.input.clear();
        state.attention_error = None;
        return;
    }
    let answers = state
        .attention_answer_input
        .take()
        .map(|draft| draft.answers)
        .unwrap_or_default();
    queue_selected_response(
        state,
        crate::api::schema::MissionResponseDecision::Answer,
        answers,
    );
}

fn queue_selected_response(
    state: &mut AppState,
    decision: crate::api::schema::MissionResponseDecision,
    answers: BTreeMap<String, Vec<String>>,
) {
    let Some(item) = state.attention_items.get(state.attention_selected) else {
        return;
    };
    if !matches!(item.state, crate::api::schema::AttentionStateV1::Open) {
        state.attention_error = Some("This request is no longer open".into());
        return;
    }
    if item.response_capability != crate::api::schema::AttentionResponseCapabilityV1::Reliable {
        state.attention_error = Some("Open the originating pane to answer this request".into());
        return;
    }
    if decision == crate::api::schema::MissionResponseDecision::ApproveForSession
        && item.risk == crate::api::schema::AttentionRiskV1::Critical
    {
        state.attention_error = Some("Critical requests can only be approved once".into());
        return;
    }
    state.request_attention_response = Some(crate::api::schema::MissionRespondParams {
        mission_id: item.mission_id.clone(),
        run_id: item.run_id.clone(),
        attention_id: item.attention_id.clone(),
        decision,
        answers,
    });
    state.attention_error = None;
}

pub(crate) fn insert_attention_answer_text(state: &mut AppState, text: &str) -> bool {
    let Some(answer) = state.attention_answer_input.as_mut() else {
        return false;
    };
    let remaining = (4 * 1024_usize).saturating_sub(answer.input.len());
    answer.input.extend(
        text.chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .take(remaining),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attention_item(
        risk: crate::api::schema::AttentionRiskV1,
    ) -> crate::api::schema::AttentionItemV1 {
        crate::api::schema::AttentionItemV1 {
            schema_version: crate::api::schema::ContractVersionV1,
            attention_id: "attention-1".into(),
            mission_id: "mission-1".into(),
            run_id: "run-1".into(),
            session_id: "session-1".into(),
            pane: crate::api::schema::AttentionPaneTargetV1 {
                workspace_id: "workspace-1".into(),
                pane_id: "pane-1".into(),
            },
            kind: crate::api::schema::AttentionKindV1::PermissionRequest,
            requested_action: "Run the test suite".into(),
            scope: "cargo test".into(),
            risk,
            provider: crate::api::schema::MissionProvider::Codex,
            source: crate::api::schema::AttentionSourceV1::ProviderApi,
            response_capability: crate::api::schema::AttentionResponseCapabilityV1::Reliable,
            questions: Vec::new(),
            created_at_millis: 1,
            expires_at_millis: None,
            occurrence_count: 1,
            unread: true,
            state: crate::api::schema::AttentionStateV1::Open,
            delivery: crate::api::schema::AttentionDeliveryStateV1::NotRequested,
        }
    }

    #[test]
    fn mission_surface_navigation_is_layered_and_escape_returns_one_level() {
        let mut state = AppState::test_new();
        state.mode = Mode::MissionInspector;

        handle_mission_inspector_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        );
        assert_eq!(state.mode, Mode::ProofReview);

        handle_proof_review_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(state.mode, Mode::MissionInspector);

        handle_mission_inspector_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(state.mode, Mode::Navigator);
    }

    #[test]
    fn mission_inspector_cycles_structured_plugin_tabs_and_queues_refresh() {
        let mut state = AppState::test_new();
        state.selected_mission_id = Some("mission-1".into());
        state.installed_plugins.clear();
        state.installed_plugins.insert(
            "example.review".into(),
            crate::api::schema::InstalledPluginInfo {
                manifest_version: 2,
                plugin_id: "example.review".into(),
                name: "Review".into(),
                version: "1.0.0".into(),
                min_nagi_version: crate::build_info::BASE_VERSION.into(),
                description: None,
                manifest_path: "/tmp/review/nagi-plugin.toml".into(),
                plugin_root: "/tmp/review".into(),
                enabled: true,
                runtime: crate::api::schema::PluginRuntimeV2::WasiComponent,
                entrypoint: Some("/tmp/review/plugin.wasm".into()),
                requested_capabilities: vec!["mission.read".into()],
                native_trusted: false,
                platforms: None,
                build: Vec::new(),
                actions: vec![crate::api::schema::PluginManifestAction {
                    id: "review-current".into(),
                    title: "Review current mission".into(),
                    description: None,
                    contexts: vec![crate::api::schema::PluginActionContext::Mission],
                    platforms: None,
                    command: Vec::new(),
                }],
                events: Vec::new(),
                panes: Vec::new(),
                link_handlers: Vec::new(),
                inspector_tabs: vec![crate::api::schema::PluginInspectorTabContributionV2 {
                    id: "risk".into(),
                    title: "Risk".into(),
                    source: "review-current".into(),
                }],
                source: Default::default(),
                warnings: Vec::new(),
            },
        );

        handle_mission_inspector_key(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(state.mission_inspector_tab, 1);
        assert_eq!(
            state.plugin_inspector_active_key.as_deref(),
            Some("example.review:risk")
        );
        let refresh = state.request_plugin_inspector_refresh.as_ref().unwrap();
        assert_eq!(refresh.mission_id, "mission-1");
        assert_eq!(refresh.source, "review-current");

        handle_mission_inspector_key(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(state.mission_inspector_tab, 0);
        assert!(state.request_plugin_inspector_refresh.is_none());
        assert!(state.plugin_inspector_active_key.is_none());
    }

    #[test]
    fn mission_handoff_requires_preview_and_explicit_write_consent() {
        let mut state = AppState::test_new();
        let mut mission: crate::api::schema::MissionViewV1 = serde_json::from_str(include_str!(
            "../../../tests/fixtures/api/mission-view-v1.json"
        ))
        .unwrap();
        mission.status = crate::api::schema::MissionStatus::Blocked;
        mission.unresolved_attention_count = 0;
        mission.run.as_mut().unwrap().provider = crate::api::schema::MissionProvider::Codex;
        state.selected_mission_id = Some(mission.mission_id.clone());
        state.mission_views.push(mission);
        state.mode = Mode::MissionInspector;

        handle_mission_inspector_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
        );
        assert_eq!(state.mode, Mode::MissionHandoff);
        assert_eq!(
            state.request_mission_handoff_preview.as_ref().unwrap().to,
            crate::api::schema::MissionProvider::ClaudeCode
        );

        let draft = state.mission_handoff.as_mut().unwrap();
        draft.loading = false;
        draft.artifact = Some(serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "artifact_sha256": "a".repeat(64),
            "generated_at_millis": 42,
            "mission_id": draft.mission_id,
            "source_run_id": "run-source",
            "suggested_run_id": "run-target",
            "source_provider": "codex",
            "target_provider": "claude_code",
            "repository_path": "/repo",
            "worktree_path": "/repo",
            "base_revision": "b".repeat(40),
            "head_revision": "c".repeat(40),
            "objective": "Continue safely",
            "acceptance_criteria": ["The target provider continues"],
            "diff": {"workspace_digest": "d".repeat(64), "dirty": false, "changed_paths": [], "stat": ""},
            "decisions": [],
            "checks": [],
            "selected_logs": [],
            "warnings": []
        })).unwrap());

        handle_mission_handoff_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(state.request_mission_handoff_start.is_none());
        assert!(state
            .mission_handoff
            .as_ref()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("write scope"));

        handle_mission_handoff_key(
            &mut state,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        handle_mission_handoff_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        let request = state.request_mission_handoff_start.as_ref().unwrap();
        assert_eq!(request.params.artifact_sha256, "a".repeat(64));
        assert!(request.workspace_write_confirmed);
    }

    #[test]
    fn proof_review_close_queues_only_a_ready_mission() {
        let mut state = AppState::test_new();
        let mut mission: crate::api::schema::MissionViewV1 = serde_json::from_str(include_str!(
            "../../../tests/fixtures/api/mission-view-v1.json"
        ))
        .unwrap();
        mission.status = crate::api::schema::MissionStatus::ReadyToClose;
        state.selected_mission_id = Some(mission.mission_id.clone());
        state.mission_views.push(mission);

        handle_proof_review_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );

        assert_eq!(
            state.request_mission_close.as_ref().unwrap().mission_id,
            state.selected_mission_id.as_deref().unwrap()
        );
        assert!(state.mission_action_error.is_none());
    }

    #[test]
    fn new_mission_requires_write_consent_and_builds_literal_argv() {
        let repository = tempfile::tempdir().unwrap();
        let recipe = crate::project_recipe::detect(repository.path());
        let mut state = AppState::test_new();
        state.mode = Mode::NewMission;
        state.new_mission = Some(NewMissionDraft {
            step: NewMissionStep::Confirm,
            repository_path: repository.path().to_path_buf(),
            recipe,
            project_recipe_summary: None,
            objective: "Preserve the requested page after login".into(),
            criteria: "Redirect test passes; URL remains intact".into(),
            proof_command: "cargo test --package 'nagi core'".into(),
            provider_index: 1,
            workspace_write_confirmed: false,
            error: None,
        });

        handle_new_mission_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(state.request_new_mission.is_none());
        assert!(state
            .new_mission
            .as_ref()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("write scope"));

        handle_new_mission_key(
            &mut state,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        handle_new_mission_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        let launch = state.request_new_mission.as_ref().unwrap();
        let crate::api::schema::MissionCheck::Command { program, args, .. } =
            &launch.configure.checks[0]
        else {
            panic!("expected command check")
        };
        assert_eq!(program, "cargo");
        assert_eq!(args, &["test", "--package", "nagi core"]);
        assert_eq!(
            launch.start.provider,
            crate::api::schema::MissionProvider::ClaudeCode
        );
        assert!(launch.workspace_write_confirmed);
        assert!(launch.branch.starts_with("mission/"));
        assert_eq!(state.mode, Mode::Navigator);
    }

    #[test]
    fn attention_approve_once_queues_a_typed_server_intent() {
        let mut state = AppState::test_new();
        state
            .attention_items
            .push(attention_item(crate::api::schema::AttentionRiskV1::High));
        state.open_attention_inbox();

        handle_attention_inbox_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        );

        let request = state.request_attention_response.as_ref().unwrap();
        assert_eq!(request.mission_id, "mission-1");
        assert_eq!(request.attention_id, "attention-1");
        assert_eq!(
            request.decision,
            crate::api::schema::MissionResponseDecision::ApproveOnce
        );
    }

    #[test]
    fn critical_attention_never_offers_session_wide_consent() {
        let mut state = AppState::test_new();
        state.attention_items.push(attention_item(
            crate::api::schema::AttentionRiskV1::Critical,
        ));
        state.open_attention_inbox();

        handle_attention_inbox_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
        );

        assert!(state.request_attention_response.is_none());
        assert_eq!(
            state.attention_error.as_deref(),
            Some("Critical requests can only be approved once")
        );
    }

    #[test]
    fn multi_question_form_preserves_exact_provider_keys() {
        let mut state = AppState::test_new();
        let mut item = attention_item(crate::api::schema::AttentionRiskV1::Low);
        item.kind = crate::api::schema::AttentionKindV1::ProviderQuestion;
        item.questions = vec![
            crate::api::schema::AttentionQuestionV1 {
                id: "database-id".into(),
                header: "Database".into(),
                prompt: "Which database?".into(),
                options: Vec::new(),
                multiple: false,
                custom_allowed: true,
            },
            crate::api::schema::AttentionQuestionV1 {
                id: "region-id".into(),
                header: "Regions".into(),
                prompt: "Which regions?".into(),
                options: Vec::new(),
                multiple: true,
                custom_allowed: true,
            },
        ];
        state.attention_items.push(item);
        state.open_attention_inbox();

        handle_attention_inbox_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        );
        state.attention_answer_input.as_mut().unwrap().input = "Postgres".into();
        handle_attention_inbox_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(
            state
                .attention_answer_input
                .as_ref()
                .map(|draft| draft.question_index),
            Some(1)
        );
        state.attention_answer_input.as_mut().unwrap().input = "eu, us, eu".into();
        handle_attention_inbox_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        let request = state.request_attention_response.unwrap();
        assert_eq!(request.answers["database-id"], ["Postgres"]);
        assert_eq!(request.answers["region-id"], ["eu", "us"]);
    }
}
