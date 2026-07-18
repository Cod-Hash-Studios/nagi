use crate::{
    api::schema::{
        ErrorBody, ErrorResponse, Method, MissionInfo, MissionProvider, MissionProviderMode,
        MissionRunInfo, MissionStatus as WireMissionStatus, MissionSummary, ResponseResult,
        SuccessResponse,
    },
    mission::{
        evidence::{ArtifactRequirement, CheckDeclaration, CommandSpec, PathRule},
        model::{MissionDefinition, MissionStatus, ProviderKind, ProviderMode},
        runtime::{ConfigureMission, CreateMission, MissionRuntime, MissionRuntimeError},
        store::{validate_mission_id, MissionStoreError, MissionView},
    },
};

pub(crate) struct MissionApiOutcome {
    pub(crate) response: String,
    pub(crate) changed: bool,
}

pub(crate) fn handle(
    runtime: &mut MissionRuntime,
    request_id: &str,
    method: &Method,
) -> Option<MissionApiOutcome> {
    if matches!(
        method,
        Method::MissionCreate(_)
            | Method::MissionList(_)
            | Method::MissionGet(_)
            | Method::MissionConfigure(_)
    ) && !runtime.is_available()
    {
        let response = serde_json::to_string(&ErrorResponse {
            id: request_id.to_owned(),
            error: ErrorBody {
                code: error_code(&MissionRuntimeError::FeatureUnavailable).to_owned(),
                message: MissionRuntimeError::FeatureUnavailable.to_string(),
            },
        })
        .unwrap_or_else(|_| {
            r#"{"id":"","error":{"code":"serialization_failed","message":"mission response serialization failed"}}"#
                .to_owned()
        });
        return Some(MissionApiOutcome {
            response,
            changed: false,
        });
    }
    let (result, changed) = match method {
        Method::MissionCreate(params) => {
            let outcome = runtime.create_mission(CreateMission {
                mission_id: params.mission_id.clone(),
                title: params.title.clone(),
                repository_path: params.repository_path.clone(),
                objective: params.objective.clone(),
                acceptance_criteria: params.acceptance_criteria.clone(),
                at_millis: now_millis(),
            });
            let changed = outcome.as_ref().is_ok_and(|outcome| outcome.created);
            (
                outcome.map(|outcome| ResponseResult::MissionCreated {
                    mission: mission_info(outcome.mission),
                    created: outcome.created,
                }),
                changed,
            )
        }
        Method::MissionList(_) => (
            Ok(ResponseResult::MissionList {
                missions: runtime
                    .missions()
                    .into_iter()
                    .map(mission_summary)
                    .collect(),
            }),
            false,
        ),
        Method::MissionGet(target) => {
            let result = validate_mission_id(&target.mission_id)
                .map_err(MissionRuntimeError::from)
                .and_then(|()| {
                    runtime
                        .mission(&target.mission_id)
                        .map(|mission| ResponseResult::MissionInfo {
                            mission: mission_info(mission),
                        })
                        .ok_or(MissionRuntimeError::MissionMissing)
                });
            (result, false)
        }
        Method::MissionConfigure(params) => {
            let outcome = configure_declarations(runtime, params).and_then(|declarations| {
                runtime.configure_mission(ConfigureMission {
                    mission_id: params.mission_id.clone(),
                    declarations,
                    at_millis: now_millis(),
                })
            });
            let changed = outcome.as_ref().is_ok_and(|outcome| outcome.configured);
            let result = outcome.map(|outcome| ResponseResult::MissionConfigured {
                mission: mission_info(outcome.mission),
                configured: outcome.configured,
            });
            (result, changed)
        }
        _ => return None,
    };

    let response = match result {
        Ok(result) => serde_json::to_string(&SuccessResponse {
            id: request_id.to_owned(),
            result,
        }),
        Err(error) => serde_json::to_string(&ErrorResponse {
            id: request_id.to_owned(),
            error: ErrorBody {
                code: error_code(&error).to_owned(),
                message: error.to_string(),
            },
        }),
    }
    .unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"serialization_failed","message":"mission response serialization failed"}}"#
            .to_owned()
    });

    Some(MissionApiOutcome { response, changed })
}

fn configure_declarations(
    runtime: &MissionRuntime,
    params: &crate::api::schema::MissionConfigureParams,
) -> Result<Vec<CheckDeclaration>, MissionRuntimeError> {
    use crate::api::schema::{MissionCheck, MissionPathRule};

    let mission = runtime
        .mission(&params.mission_id)
        .ok_or(MissionRuntimeError::MissionMissing)?;
    let criterion_ids = MissionDefinition::criterion_ids(&mission.acceptance_criteria);
    params
        .checks
        .iter()
        .map(|check| {
            let covers = |indexes: &[usize]| {
                indexes
                    .iter()
                    .map(|index| criterion_ids.get(*index).cloned())
                    .collect::<Option<Vec<_>>>()
                    .ok_or(MissionRuntimeError::InvalidClosurePlan)
            };
            match check {
                MissionCheck::Command {
                    id,
                    program,
                    args,
                    cwd,
                    relevant_paths,
                    required_artifacts,
                    include_ignored,
                    required,
                    covers: covered_indexes,
                } => {
                    let paths = relevant_paths
                        .iter()
                        .map(|rule| match rule {
                            MissionPathRule::All => PathRule::All,
                            MissionPathRule::Exact { path } => PathRule::exact(path),
                            MissionPathRule::Prefix { prefix } => PathRule::prefix(prefix),
                        })
                        .collect();
                    let artifacts = required_artifacts
                        .iter()
                        .map(ArtifactRequirement::new)
                        .collect();
                    let mut declaration = CheckDeclaration::command(
                        id,
                        CommandSpec::new(program, args.clone(), cwd),
                        paths,
                        artifacts,
                    )
                    .include_ignored(*include_ignored)
                    .covers(covers(covered_indexes)?);
                    if !*required {
                        declaration = declaration.optional();
                    }
                    Ok(declaration)
                }
                MissionCheck::Manual {
                    id,
                    reviewers,
                    allow_override,
                    required,
                    covers: covered_indexes,
                } => {
                    let mut declaration = CheckDeclaration::manual(id)
                        .reviewed_by(reviewers.clone())
                        .covers(covers(covered_indexes)?);
                    if *allow_override {
                        declaration = declaration.allow_manual_override();
                    }
                    if !*required {
                        declaration = declaration.optional();
                    }
                    Ok(declaration)
                }
            }
        })
        .collect()
}

pub(crate) fn mission_info(view: MissionView) -> MissionInfo {
    MissionInfo {
        mission_id: view.mission_id,
        title: view.title,
        repository_path: view.repository_path,
        objective: view.objective,
        acceptance_criteria: view.acceptance_criteria,
        closure_configured: !view.check_declarations.is_empty(),
        check_count: view.check_declarations.len(),
        status: wire_status(view.status),
        run: view.run.map(|run| MissionRunInfo {
            run_id: run.run_id,
            provider: wire_provider(run.provider),
            mode: wire_provider_mode(run.mode),
            worktree_path: run.worktree_path,
            base_revision: run.base_revision,
        }),
        unresolved_attention_count: view.unresolved_attention_count,
        updated_at_millis: view.updated_at_millis,
    }
}

fn mission_summary(view: MissionView) -> MissionSummary {
    MissionSummary {
        mission_id: view.mission_id,
        title: view.title,
        repository_path: view.repository_path,
        status: wire_status(view.status),
        unresolved_attention_count: view.unresolved_attention_count,
        updated_at_millis: view.updated_at_millis,
    }
}

const fn wire_status(status: MissionStatus) -> WireMissionStatus {
    match status {
        MissionStatus::Draft => WireMissionStatus::Draft,
        MissionStatus::Preparing => WireMissionStatus::Preparing,
        MissionStatus::Active => WireMissionStatus::Active,
        MissionStatus::ReviewRequired => WireMissionStatus::ReviewRequired,
        MissionStatus::ReadyToClose => WireMissionStatus::ReadyToClose,
        MissionStatus::Blocked => WireMissionStatus::Blocked,
        MissionStatus::Failed => WireMissionStatus::Failed,
        MissionStatus::Archived => WireMissionStatus::Archived,
    }
}

const fn wire_provider(provider: ProviderKind) -> MissionProvider {
    match provider {
        ProviderKind::Codex => MissionProvider::Codex,
        ProviderKind::ClaudeCode => MissionProvider::ClaudeCode,
        ProviderKind::OpenCode => MissionProvider::OpenCode,
    }
}

const fn wire_provider_mode(mode: ProviderMode) -> MissionProviderMode {
    match mode {
        ProviderMode::Managed => MissionProviderMode::Managed,
        ProviderMode::Passthrough => MissionProviderMode::Passthrough,
    }
}

pub(crate) fn error_code(error: &MissionRuntimeError) -> &'static str {
    match error {
        MissionRuntimeError::FeatureUnavailable => "feature_unavailable",
        MissionRuntimeError::MissionMissing => "mission_not_found",
        MissionRuntimeError::MissionConflict
        | MissionRuntimeError::ClosureConflict
        | MissionRuntimeError::Store(MissionStoreError::MissionAlreadyExists(_))
        | MissionRuntimeError::Store(MissionStoreError::EventIdConflict(_)) => "mission_conflict",
        MissionRuntimeError::RepositoryUnavailable
        | MissionRuntimeError::RepositoryNotGit
        | MissionRuntimeError::RepositoryMustBeRoot => "invalid_repository",
        MissionRuntimeError::Store(MissionStoreError::InvalidIdentifier { .. })
        | MissionRuntimeError::Store(MissionStoreError::InvalidText { .. })
        | MissionRuntimeError::Store(MissionStoreError::RepositoryPathNotAbsolute)
        | MissionRuntimeError::Store(MissionStoreError::InvalidAcceptanceCriteria) => {
            "invalid_mission"
        }
        MissionRuntimeError::InvalidClosurePlan
        | MissionRuntimeError::ClosureMissing
        | MissionRuntimeError::Store(MissionStoreError::InvalidClosurePlan)
        | MissionRuntimeError::Store(MissionStoreError::ClosureMissing) => "invalid_closure",
        _ => "mission_runtime_error",
    }
}

pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
