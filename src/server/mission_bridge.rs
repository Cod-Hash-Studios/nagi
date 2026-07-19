use std::collections::BTreeSet;

use crate::{
    api::schema::{
        ContractVersionV1, ErrorBody, ErrorResponse, Method, MissionCheckKindV1,
        MissionCheckStatusV1, MissionCheckSummaryV1, MissionCriterionCoverageV1,
        MissionCriterionSummaryV1, MissionEvidenceAssessmentV1, MissionEvidenceKindV1,
        MissionEvidenceSummaryV1, MissionProvider, MissionProviderMode, MissionRunViewV1,
        MissionStatus as WireMissionStatus, MissionSummary, MissionViewV1, ProofArtifactV1,
        ProofClosureDecisionV1, ProofEvidenceSourceV1, ProofEvidenceV1, ProofIdentityV1,
        ProofReceiptV1, ResponseResult, SuccessResponse,
    },
    mission::{
        evidence::{ArtifactRequirement, CheckDeclaration, CommandSpec, EvidenceRecord, PathRule},
        handoff::{build_preview, MissionHandoffError},
        model::{MissionDefinition, MissionStatus, ProviderKind, ProviderMode},
        proof::{
            digest_attention, digest_evidence, proof_seal_digest, proof_subject_digest, ClosurePlan,
        },
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
            | Method::MissionProofGet(_)
            | Method::MissionHandoffPreview(_)
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
                    mission: mission_view(outcome.mission),
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
                            mission: mission_view(mission),
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
                mission: mission_view(outcome.mission),
                configured: outcome.configured,
            });
            (result, changed)
        }
        Method::MissionProofGet(target) => (
            portable_proof_receipt(runtime, &target.mission_id)
                .map(|receipt| ResponseResult::MissionProof { receipt }),
            false,
        ),
        Method::MissionHandoffPreview(params) => {
            let result = validate_mission_id(&params.mission_id)
                .map_err(MissionRuntimeError::from)
                .and_then(|()| {
                    let mission = runtime
                        .mission(&params.mission_id)
                        .ok_or(MissionRuntimeError::MissionMissing)?;
                    let checks = mission_view(mission.clone()).checks;
                    let attention = runtime.attention_items();
                    let artifact =
                        build_preview(&mission, &attention, checks, params.to, now_millis())?;
                    Ok(ResponseResult::MissionHandoffPreview { artifact })
                });
            (result, false)
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

fn portable_proof_receipt(
    runtime: &MissionRuntime,
    mission_id: &str,
) -> Result<ProofReceiptV1, MissionRuntimeError> {
    validate_mission_id(mission_id)?;
    let mission = runtime
        .mission(mission_id)
        .ok_or(MissionRuntimeError::MissionMissing)?;
    let (receipt, decision) = match mission.status {
        MissionStatus::ReadyToClose => (
            mission
                .ready_receipt
                .as_ref()
                .ok_or(MissionRuntimeError::ProofUnavailable)?,
            ProofClosureDecisionV1::ReadyToClose,
        ),
        MissionStatus::Archived => (
            mission
                .archive_receipt
                .as_ref()
                .ok_or(MissionRuntimeError::ProofUnavailable)?,
            ProofClosureDecisionV1::Archived,
        ),
        _ => return Err(MissionRuntimeError::ProofUnavailable),
    };
    let run = mission
        .run
        .as_ref()
        .ok_or(MissionRuntimeError::ProofUnavailable)?;
    let pack_digest = mission
        .latest_evidence_pack_digest
        .as_deref()
        .ok_or(MissionRuntimeError::ProofUnavailable)?;
    let pack = runtime.load_evidence_pack(pack_digest)?;
    if pack.mission_id() != mission.mission_id || pack.run_id() != run.run_id {
        return Err(MissionRuntimeError::ProofBindingMismatch);
    }
    let definition = MissionDefinition::new(
        &mission.mission_id,
        &mission.title,
        &mission.repository_path,
        &mission.objective,
        mission.acceptance_criteria.clone(),
    )
    .map_err(|_| MissionRuntimeError::InvalidClosurePlan)?;
    let closure_plan = ClosurePlan::new(
        &definition.acceptance_criterion_ids(),
        &mission.check_declarations,
    )
    .map_err(|_| MissionRuntimeError::InvalidClosurePlan)?;
    let attention_digest = digest_attention(&BTreeSet::new());
    let evidence_digest = digest_evidence(pack.records());
    let subject_digest =
        proof_subject_digest(pack.identity(), pack.current_workspace(), &closure_plan);
    let seal_digest = proof_seal_digest(
        pack.identity(),
        pack.current_workspace(),
        &closure_plan,
        &attention_digest,
        &evidence_digest,
        receipt.authority_head_digest(),
        receipt.lease_digest(),
        receipt.authority_sequence(),
        receipt.verified_at_millis(),
    );
    if receipt.subject_digest() != subject_digest || receipt.seal_digest() != seal_digest {
        return Err(MissionRuntimeError::ProofBindingMismatch);
    }

    let fresh_evidence = mission
        .check_declarations
        .iter()
        .filter_map(|declaration| {
            if pack.summaries().get(declaration.id())
                != Some(&crate::mission::evidence::EvidenceStatus::Passed)
            {
                return None;
            }
            let record = pack.records().get(declaration.id())?;
            proof_evidence(declaration, record)
        })
        .collect::<Vec<_>>();
    if fresh_evidence.is_empty() {
        return Err(MissionRuntimeError::ProofBindingMismatch);
    }

    Ok(ProofReceiptV1 {
        schema_version: ContractVersionV1,
        identity: ProofIdentityV1 {
            mission_id: mission.mission_id,
            run_id: run.run_id.clone(),
            repository_identity: pack.identity().repository_identity().to_owned(),
            worktree_identity: pack.identity().worktree_identity().to_owned(),
            base_revision: pack.identity().base_revision().to_owned(),
        },
        head_revision: pack.current_workspace().head_revision().to_owned(),
        workspace_digest: pack.current_workspace().digest(),
        criteria_digest: closure_plan.criteria_digest().to_owned(),
        checkset_digest: closure_plan.checkset_digest().to_owned(),
        attention_digest,
        evidence_digest,
        subject_digest,
        seal_digest,
        authority_head_digest: receipt.authority_head_digest().to_owned(),
        lease_digest: receipt.lease_digest().to_owned(),
        authority_sequence: receipt.authority_sequence(),
        verified_at_millis: receipt.verified_at_millis(),
        decision,
        fresh_evidence,
    })
}

fn proof_evidence(
    declaration: &CheckDeclaration,
    record: &EvidenceRecord,
) -> Option<ProofEvidenceV1> {
    let (declaration_digest, source, artifacts) = match record {
        EvidenceRecord::Command(evidence) => (
            evidence.declaration_digest().to_owned(),
            ProofEvidenceSourceV1::Command {
                program: evidence.command().program().to_owned(),
                args: evidence.command().args().to_vec(),
                cwd: evidence.command().cwd().to_owned(),
                exit_code: evidence.exit_code(),
                started_at_millis: evidence.started_at_millis(),
                finished_at_millis: evidence.finished_at_millis(),
            },
            evidence
                .artifacts()
                .iter()
                .map(|artifact| ProofArtifactV1 {
                    path: artifact.path().to_owned(),
                    content_digest: artifact.content_hash().to_owned(),
                    media_type: artifact.media_type().to_owned(),
                })
                .collect(),
        ),
        EvidenceRecord::Manual(evidence) => (
            evidence.declaration_digest().to_owned(),
            ProofEvidenceSourceV1::Manual {
                reviewer: evidence.author().to_owned(),
                reason: evidence.reason().to_owned(),
                is_override: evidence.is_override(),
                recorded_at_millis: evidence.recorded_at_millis(),
            },
            Vec::new(),
        ),
        EvidenceRecord::ProviderClaim(_) => return None,
    };
    Some(ProofEvidenceV1 {
        check_id: declaration.id().to_owned(),
        declaration_digest,
        evidence_digest: record.digest(),
        required: declaration.is_required(),
        covered_criterion_ids: declaration.covered_criteria().iter().cloned().collect(),
        source,
        artifacts,
    })
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

pub(crate) fn mission_view(view: MissionView) -> MissionViewV1 {
    use std::collections::BTreeMap;

    let criterion_ids = MissionDefinition::criterion_ids(&view.acceptance_criteria);
    let evidence_by_check = view
        .evidence
        .iter()
        .map(|evidence| (evidence.check_id.as_str(), evidence))
        .collect::<BTreeMap<_, _>>();
    let criteria = view
        .acceptance_criteria
        .iter()
        .zip(&criterion_ids)
        .map(|(description, criterion_id)| {
            let required_check_ids = view
                .check_declarations
                .iter()
                .filter(|declaration| {
                    declaration.is_required()
                        && declaration.covered_criteria().contains(criterion_id)
                })
                .map(|declaration| declaration.id().to_owned())
                .collect::<Vec<_>>();
            MissionCriterionSummaryV1 {
                criterion_id: Some(criterion_id.clone()),
                description: description.clone(),
                coverage: if required_check_ids.is_empty() {
                    MissionCriterionCoverageV1::Uncovered
                } else {
                    MissionCriterionCoverageV1::Covered
                },
                required_check_ids,
            }
        })
        .collect();
    let checks = view
        .check_declarations
        .iter()
        .map(|declaration| {
            let status = evidence_by_check
                .get(declaration.id())
                .map(|evidence| match evidence.status {
                    crate::mission::evidence::EvidenceStatus::Passed => {
                        MissionCheckStatusV1::Passed
                    }
                    crate::mission::evidence::EvidenceStatus::Failed => {
                        MissionCheckStatusV1::Failed
                    }
                    crate::mission::evidence::EvidenceStatus::Stale => MissionCheckStatusV1::Stale,
                })
                .unwrap_or_else(|| {
                    if declaration.is_manual() {
                        MissionCheckStatusV1::ManualMissing
                    } else {
                        MissionCheckStatusV1::Missing
                    }
                });
            MissionCheckSummaryV1 {
                check_id: declaration.id().to_owned(),
                kind: if declaration.is_manual() {
                    MissionCheckKindV1::Manual
                } else {
                    MissionCheckKindV1::Command
                },
                required: declaration.is_required(),
                covered_criterion_ids: declaration.covered_criteria().iter().cloned().collect(),
                status,
            }
        })
        .collect();
    let evidence = view
        .evidence
        .iter()
        .map(|item| {
            let declaration = view
                .check_declarations
                .iter()
                .find(|declaration| declaration.id() == item.check_id);
            MissionEvidenceSummaryV1 {
                check_id: item.check_id.clone(),
                kind: if declaration.is_some_and(CheckDeclaration::is_manual) {
                    MissionEvidenceKindV1::Manual
                } else {
                    MissionEvidenceKindV1::Command
                },
                assessment: match item.status {
                    crate::mission::evidence::EvidenceStatus::Passed => {
                        MissionEvidenceAssessmentV1::Passed
                    }
                    crate::mission::evidence::EvidenceStatus::Failed => {
                        MissionEvidenceAssessmentV1::Failed
                    }
                    crate::mission::evidence::EvidenceStatus::Stale => {
                        MissionEvidenceAssessmentV1::Stale
                    }
                },
                workspace_digest: item.workspace_hash.clone(),
                recorded_at_millis: item.updated_at_millis,
                duration_millis: None,
                exit_code: None,
                artifact_count: 0,
                reviewer: None,
                manual_override: None,
                source: None,
            }
        })
        .collect();
    let run = view.run.map(mission_run_view);
    let run_history = view.run_history.into_iter().map(mission_run_view).collect();
    MissionViewV1 {
        schema_version: ContractVersionV1,
        mission_id: view.mission_id,
        title: view.title,
        repository_path: view.repository_path,
        objective: view.objective,
        criteria,
        closure_configured: !view.check_declarations.is_empty(),
        declared_check_count: u32::try_from(view.check_declarations.len()).unwrap_or(32),
        checks,
        evidence,
        evidence_pack_digest: view.latest_evidence_pack_digest,
        details_available: true,
        status: wire_status(view.status),
        run,
        run_history,
        unresolved_attention_count: u32::try_from(view.unresolved_attention_count)
            .unwrap_or(100_000),
        updated_at_millis: view.updated_at_millis,
    }
}

fn mission_run_view(run: crate::mission::store::MissionRunView) -> MissionRunViewV1 {
    MissionRunViewV1 {
        run_id: run.run_id,
        provider: wire_provider(run.provider),
        mode: wire_provider_mode(run.mode),
        worktree_path: run.worktree_path,
        base_revision: run.base_revision,
        execute_declared_checks: run.execute_declared_checks,
        execute_project_recipe: run.execute_project_recipe,
        handoff_from_run_id: run.handoff_from_run_id,
        handoff_artifact_sha256: run.handoff_artifact_sha256,
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
        ProviderKind::Acp => MissionProvider::Acp,
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
        MissionRuntimeError::ProofUnavailable => "proof_not_ready",
        MissionRuntimeError::ProofBindingMismatch => "proof_binding_mismatch",
        MissionRuntimeError::Handoff(
            MissionHandoffError::InvalidSourceState | MissionHandoffError::SourceRunMissing,
        ) => "invalid_handoff_state",
        MissionRuntimeError::Handoff(MissionHandoffError::SameProvider) => {
            "invalid_handoff_provider"
        }
        MissionRuntimeError::Handoff(MissionHandoffError::UnresolvedAttention)
        | MissionRuntimeError::Store(MissionStoreError::HandoffAttentionUnresolved) => {
            "handoff_attention_unresolved"
        }
        MissionRuntimeError::Store(MissionStoreError::HandoffSameProvider) => {
            "invalid_handoff_provider"
        }
        MissionRuntimeError::Store(MissionStoreError::RunAlreadyExists) => "run_conflict",
        MissionRuntimeError::Store(MissionStoreError::RunMismatch) => "run_mismatch",
        MissionRuntimeError::Handoff(_) => "handoff_snapshot_failed",
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
