use std::collections::BTreeMap;

use proptest::prelude::*;

use super::{
    attention::{
        AttentionDecision, AttentionError, AttentionEvent, AttentionInbox, AttentionKind,
        AttentionRisk, AttentionStatus, PaneTarget, ProviderResponseIntent, ProviderResponseToken,
        ResponseCapability,
    },
    claims::{ClaimRequestId, LeaseOwner, ReleaseOutcome, WorktreeClaimRegistry},
    evidence::{
        ArtifactEvidence, ArtifactRequirement, CheckDeclaration, CommandEvidence, CommandSpec,
        EvidenceRecord, EvidenceStatus, FileDisposition, FileFingerprint, ManualEvidence,
        MissionReadiness, PathRule, ProviderClaim, WorkspaceSnapshot,
    },
    model::{
        ArchiveProof, MissionDefinition, MissionError, MissionLifecycle, MissionStatus,
        ProviderKind, ProviderMode, ReadyProof, RunTarget,
    },
    proof::{CheckProofStatus, ProofIdentity, ProofReport},
    run_state::{
        Confidence, ObservationSource, SessionObservation, SessionSnapshot, SessionStatus,
    },
    runtime::{
        AuthoritySnapshot, ConfigureMission, ContinueRun, CreateMission, MissionRuntime,
        MissionRuntimeError, StartRun,
    },
    store::{
        MissionStore, MissionStoreReader, PersistableMissionEvent, PersistedAttentionState,
        PersistedResponseRoute, PersistedResponseState, ResponseAttemptKey, ResponseFailureCode,
        ResponseFailureDisposition,
    },
};

#[test]
fn disabled_mission_runtime_returns_typed_feature_unavailable() {
    let mut runtime = MissionRuntime::disabled();
    assert!(!runtime.is_available());
    assert!(matches!(
        runtime.commit(
            "event-1",
            PersistableMissionEvent::StatusChanged {
                mission_id: "mission-1".to_owned(),
                status: MissionStatus::Preparing,
                at_millis: 1,
            }
        ),
        Err(MissionRuntimeError::FeatureUnavailable)
    ));

    let outcome = crate::server::mission_bridge::handle(
        &mut runtime,
        "mission-disabled",
        &crate::api::schema::Method::MissionList(crate::api::schema::EmptyParams {}),
    )
    .unwrap();
    let response: serde_json::Value = serde_json::from_str(&outcome.response).unwrap();
    assert_eq!(response["error"]["code"], "feature_unavailable");
    assert!(!outcome.changed);
}

#[test]
fn portable_proof_is_rejected_until_the_mission_is_verified() {
    let directory = tempfile::tempdir().unwrap();
    let claims = directory.path().join("claims");
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    let mut runtime = MissionRuntime::open_owned(directory.path(), &claims).unwrap();
    runtime
        .create_mission(CreateMission {
            mission_id: "mission-unverified".into(),
            title: "Do not invent proof".into(),
            repository_path: repository.to_string_lossy().into_owned(),
            objective: "Reject a proof request before verification".into(),
            acceptance_criteria: vec!["A fresh check passes".into()],
            at_millis: 1,
        })
        .unwrap();

    let outcome = crate::server::mission_bridge::handle(
        &mut runtime,
        "proof-too-early",
        &crate::api::schema::Method::MissionProofGet(crate::api::schema::MissionTarget {
            mission_id: "mission-unverified".into(),
        }),
    )
    .unwrap();
    let response: crate::api::schema::ErrorResponse =
        serde_json::from_str(&outcome.response).unwrap();
    assert_eq!(response.error.code, "proof_not_ready");
    assert!(!outcome.changed);
}

#[cfg(unix)]
#[test]
fn mission_runtime_handoff_transfers_the_single_writer() {
    let directory = tempfile::tempdir().unwrap();
    let claim_directory = directory.path().join("claims");
    let mut source = MissionRuntime::open_owned(directory.path(), &claim_directory).unwrap();
    let fence = source.prepare_handoff().unwrap();
    let mut successor =
        MissionRuntime::observe_handoff(directory.path(), &claim_directory, fence).unwrap();

    source.relinquish_handoff().unwrap();
    successor.acquire_handoff(fence).unwrap();

    assert!(successor.is_owned());
    assert!(source.abort_handoff().is_err());
}

#[cfg(unix)]
#[test]
fn mission_runtime_can_rollback_before_commit() {
    let directory = tempfile::tempdir().unwrap();
    let mut runtime =
        MissionRuntime::open_owned(directory.path(), &directory.path().join("claims")).unwrap();

    runtime.prepare_handoff().unwrap();
    runtime.abort_handoff().unwrap();

    assert!(runtime.is_owned());
}

#[test]
fn runtime_authority_is_bound_to_the_current_journal_lease_and_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let mut runtime =
        MissionRuntime::open_owned(directory.path(), &directory.path().join("claims")).unwrap();
    runtime
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let lease = runtime
        .claim_worktree(
            LeaseOwner::new("mission-1", "run-1").unwrap(),
            &repository,
            &repository,
            ClaimRequestId::new("authority-claim").unwrap(),
        )
        .unwrap();
    let identity = ProofIdentity::new(
        "mission-1",
        "run-1",
        repository.to_string_lossy(),
        repository.to_string_lossy(),
        "a".repeat(40),
    )
    .unwrap();
    let check = command_check();
    let state = workspace("authority");
    let evidence = CommandEvidence::new(
        &check,
        &identity,
        &state,
        &state,
        0,
        10,
        20,
        vec![ArtifactEvidence::new(
            "target/report.json",
            "artifact-hash",
            "application/json",
        )],
    )
    .unwrap();
    let records = BTreeMap::from([(
        check.id().to_owned(),
        EvidenceRecord::Command(Box::new(evidence)),
    )]);
    let blockers = std::collections::BTreeSet::new();

    let authority = runtime
        .capture_authority(&lease, &identity, &state, &records, &blockers, 30)
        .unwrap();
    let wrong_identity = ProofIdentity::new(
        "mission-2",
        "run-1",
        repository.to_string_lossy(),
        repository.to_string_lossy(),
        "a".repeat(40),
    )
    .unwrap();

    assert_eq!(authority.sequence(), 1);
    assert!(runtime
        .capture_authority(&lease, &wrong_identity, &state, &records, &blockers, 31,)
        .is_err());
    assert_eq!(
        runtime.release_worktree(&lease).unwrap(),
        ReleaseOutcome::Released
    );
}

#[test]
fn sealed_runtime_commit_rejects_a_released_or_replaced_lease() {
    let directory = tempfile::tempdir().unwrap();
    let mut runtime =
        MissionRuntime::open_owned(directory.path(), &directory.path().join("claims")).unwrap();
    runtime
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    for (event_id, status, at_millis) in [
        ("event-2", MissionStatus::Preparing, 20),
        ("event-3", MissionStatus::Active, 30),
        ("event-4", MissionStatus::ReviewRequired, 40),
    ] {
        runtime
            .commit(
                event_id,
                PersistableMissionEvent::StatusChanged {
                    mission_id: "mission-1".to_owned(),
                    status,
                    at_millis,
                },
            )
            .unwrap();
    }

    let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let owner = LeaseOwner::new("mission-1", "run-1").unwrap();
    let request_id = ClaimRequestId::new("proof-claim").unwrap();
    let lease = runtime
        .claim_worktree(owner.clone(), &repository, &repository, request_id.clone())
        .unwrap();
    let check = command_check().covers(mission_definition("mission-1").acceptance_criterion_ids());
    let lifecycle = MissionLifecycle::draft(mission_definition("mission-1"), 10)
        .with_run_target(
            RunTarget::new(
                "run-1",
                ProviderKind::Codex,
                ProviderMode::Managed,
                "a".repeat(40),
                repository.to_string_lossy(),
            )
            .unwrap(),
            std::slice::from_ref(&check),
            "test",
            20,
        )
        .unwrap()
        .transition(MissionStatus::Active, "runtime", "provider ready", 30)
        .unwrap()
        .transition(
            MissionStatus::ReviewRequired,
            "runtime",
            "turn completed",
            40,
        )
        .unwrap();
    let identity = lifecycle.proof_identity().unwrap();
    let state = workspace("lease-cas");
    let evidence = CommandEvidence::new(
        &check,
        &identity,
        &state,
        &state,
        0,
        41,
        42,
        vec![ArtifactEvidence::new(
            "target/report.json",
            "artifact-hash",
            "application/json",
        )],
    )
    .unwrap();
    let records = BTreeMap::from([(
        check.id().to_owned(),
        EvidenceRecord::Command(Box::new(evidence)),
    )]);
    let blockers = std::collections::BTreeSet::new();
    let authority = runtime
        .capture_authority(&lease, &identity, &state, &records, &blockers, 43)
        .unwrap();
    let proof = lifecycle
        .evaluate_ready_proof(&[check], &records, &state, &blockers, &authority)
        .unwrap();
    let event =
        PersistableMissionEvent::mission_ready("mission-1", proof, "reviewer", "c".repeat(64), 44)
            .unwrap();

    assert_eq!(
        runtime.release_worktree(&lease).unwrap(),
        ReleaseOutcome::Released
    );
    let replacement = runtime
        .claim_worktree(owner, &repository, &repository, request_id)
        .unwrap();
    let error = runtime.commit("event-5", event).unwrap_err();
    assert_eq!(
        error.to_string(),
        "mission authority requires a current worktree lease"
    );
    assert_eq!(
        runtime.release_worktree(&replacement).unwrap(),
        ReleaseOutcome::Released
    );
}

#[test]
fn runtime_create_list_and_get_survive_restart_idempotently() {
    let directory = tempfile::tempdir().unwrap();
    let claim_directory = directory.path().join("claims");
    let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let create = |at_millis| CreateMission {
        mission_id: "mission-api-1".to_owned(),
        title: "Fix login redirect".to_owned(),
        repository_path: repository.to_string_lossy().into_owned(),
        objective: "Preserve the requested page after login".to_owned(),
        acceptance_criteria: vec!["Redirect test passes".to_owned()],
        at_millis,
    };
    let mut runtime = MissionRuntime::open_owned(directory.path(), &claim_directory).unwrap();

    let created = runtime.create_mission(create(10)).unwrap();
    let duplicate = runtime.create_mission(create(20)).unwrap();

    assert!(created.created);
    assert!(!duplicate.created);
    assert_eq!(created.mission, duplicate.mission);
    assert_eq!(runtime.missions(), vec![created.mission.clone()]);
    assert_eq!(
        runtime.mission("mission-api-1"),
        Some(created.mission.clone())
    );
    drop(runtime);

    let restored = MissionRuntime::open_owned(directory.path(), &claim_directory).unwrap();
    assert_eq!(restored.mission("mission-api-1"), Some(created.mission));
}

#[test]
fn closure_configuration_is_idempotent_durable_and_required_before_start() {
    let directory = tempfile::tempdir().unwrap();
    let claim_directory = directory.path().join("claims");
    let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let mut runtime = MissionRuntime::open_owned(directory.path(), &claim_directory).unwrap();
    runtime
        .create_mission(CreateMission {
            mission_id: "mission-closure".into(),
            title: "Persist closure".into(),
            repository_path: repository.to_string_lossy().into_owned(),
            objective: "Require durable checks".into(),
            acceptance_criteria: vec!["The configured check passes".into()],
            at_millis: 10,
        })
        .unwrap();
    let start_without_closure = runtime.start_run(StartRun {
        mission_id: "mission-closure".into(),
        run_id: "run-without-closure".into(),
        provider: ProviderKind::Codex,
        mode: ProviderMode::Managed,
        worktree_path: repository.to_string_lossy().into_owned(),
        request_id: ClaimRequestId::new("missing-closure").unwrap(),
        execute_declared_checks: false,
        execute_project_recipe: false,
        at_millis: 15,
    });
    assert!(matches!(
        start_without_closure,
        Err(super::runtime::MissionRuntimeError::ClosureMissing)
    ));

    let criterion_ids =
        MissionDefinition::criterion_ids(&["The configured check passes".to_owned()]);
    let declaration = CheckDeclaration::command(
        "test",
        CommandSpec::new("cargo", ["test"], "."),
        vec![PathRule::All],
        Vec::new(),
    )
    .covers(criterion_ids);
    let configured = runtime
        .configure_mission(ConfigureMission {
            mission_id: "mission-closure".into(),
            declarations: vec![declaration.clone()],
            at_millis: 20,
        })
        .unwrap();
    let duplicate = runtime
        .configure_mission(ConfigureMission {
            mission_id: "mission-closure".into(),
            declarations: vec![declaration],
            at_millis: 30,
        })
        .unwrap();
    assert!(configured.configured);
    assert!(!duplicate.configured);
    assert_eq!(configured.mission, duplicate.mission);
    let conflicting = runtime.configure_mission(ConfigureMission {
        mission_id: "mission-closure".into(),
        declarations: vec![CheckDeclaration::command(
            "different-test",
            CommandSpec::new("cargo", ["test"], "."),
            vec![PathRule::All],
            Vec::new(),
        )
        .covers(MissionDefinition::criterion_ids(&[
            "The configured check passes".to_owned(),
        ]))],
        at_millis: 40,
    });
    assert!(matches!(
        conflicting,
        Err(super::runtime::MissionRuntimeError::ClosureConflict)
    ));
    drop(runtime);

    let mut restored = MissionRuntime::open_owned(directory.path(), &claim_directory).unwrap();
    assert_eq!(
        restored
            .mission("mission-closure")
            .unwrap()
            .check_declarations
            .len(),
        1
    );
    let started = restored
        .start_run(StartRun {
            mission_id: "mission-closure".into(),
            run_id: "run-after-restart".into(),
            provider: ProviderKind::Codex,
            mode: ProviderMode::Managed,
            worktree_path: repository.to_string_lossy().into_owned(),
            request_id: ClaimRequestId::new("run-after-restart").unwrap(),
            execute_declared_checks: false,
            execute_project_recipe: true,
            at_millis: 50,
        })
        .unwrap();
    assert!(started
        .mission
        .run
        .as_ref()
        .is_some_and(|run| run.execute_project_recipe));
    restored.release_worktree(&started.lease).unwrap();
}

#[test]
fn provider_handoff_continues_the_same_mission_with_a_new_durable_run() {
    let directory = tempfile::tempdir().unwrap();
    let claim_directory = directory.path().join("claims");
    let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let mut runtime = MissionRuntime::open_owned(directory.path(), &claim_directory).unwrap();
    runtime
        .create_mission(CreateMission {
            mission_id: "mission-continue".into(),
            title: "Continue with another provider".into(),
            repository_path: repository.to_string_lossy().into_owned(),
            objective: "Keep the mission and its audit trail intact".into(),
            acceptance_criteria: vec!["The new provider uses the existing worktree".into()],
            at_millis: 1,
        })
        .unwrap();
    runtime
        .configure_mission(ConfigureMission {
            mission_id: "mission-continue".into(),
            declarations: vec![command_check().covers(MissionDefinition::criterion_ids(&[
                "The new provider uses the existing worktree".into(),
            ]))],
            at_millis: 2,
        })
        .unwrap();
    let source = runtime
        .start_run(StartRun {
            mission_id: "mission-continue".into(),
            run_id: "run-codex".into(),
            provider: ProviderKind::Codex,
            mode: ProviderMode::Managed,
            worktree_path: repository.to_string_lossy().into_owned(),
            request_id: ClaimRequestId::new("claim-codex").unwrap(),
            execute_declared_checks: true,
            execute_project_recipe: true,
            at_millis: 3,
        })
        .unwrap();
    runtime
        .bind_provider_session("mission-continue", "run-codex", "session-codex", 4)
        .unwrap();
    runtime
        .commit(
            "source-evidence",
            PersistableMissionEvent::EvidenceChanged {
                mission_id: "mission-continue".into(),
                check_id: "quality".into(),
                status: EvidenceStatus::Passed,
                workspace_hash: "a".repeat(64),
                at_millis: 5,
            },
        )
        .unwrap();
    runtime
        .transition_run("mission-continue", MissionStatus::Blocked, 6)
        .unwrap();
    runtime.release_worktree(&source.lease).unwrap();

    let continued = runtime
        .continue_run(ContinueRun {
            mission_id: "mission-continue".into(),
            source_run_id: "run-codex".into(),
            run_id: "run-claude".into(),
            provider: ProviderKind::ClaudeCode,
            mode: ProviderMode::Managed,
            request_id: ClaimRequestId::new("claim-claude").unwrap(),
            handoff_artifact_sha256: "b".repeat(64),
            at_millis: 7,
        })
        .unwrap();

    assert_eq!(continued.mission.status, MissionStatus::Preparing);
    let run = continued.mission.run.as_ref().unwrap();
    assert_eq!(run.run_id, "run-claude");
    assert_eq!(run.provider, ProviderKind::ClaudeCode);
    assert_eq!(run.worktree_path, repository.to_string_lossy());
    assert_eq!(run.handoff_from_run_id.as_deref(), Some("run-codex"));
    let expected_artifact_digest = "b".repeat(64);
    assert_eq!(
        run.handoff_artifact_sha256.as_deref(),
        Some(expected_artifact_digest.as_str())
    );
    assert!(run.execute_declared_checks);
    assert!(run.execute_project_recipe);
    assert_eq!(continued.mission.run_history.len(), 1);
    assert_eq!(continued.mission.run_history[0].run_id, "run-codex");
    assert!(continued
        .mission
        .evidence
        .iter()
        .all(|evidence| evidence.status == EvidenceStatus::Stale));
    runtime.release_worktree(&continued.lease).unwrap();
    drop(runtime);

    let restored = MissionRuntime::open_owned(directory.path(), &claim_directory).unwrap();
    let mission = restored.mission("mission-continue").unwrap();
    assert_eq!(mission.run.as_ref().unwrap().run_id, "run-claude");
    assert_eq!(mission.run_history[0].run_id, "run-codex");
}

#[test]
fn provider_handoff_rejects_same_provider_and_unresolved_attention() {
    let directory = tempfile::tempdir().unwrap();
    let claim_directory = directory.path().join("claims");
    let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let mut runtime = MissionRuntime::open_owned(directory.path(), &claim_directory).unwrap();
    runtime
        .create_mission(CreateMission {
            mission_id: "mission-guarded-handoff".into(),
            title: "Guard handoff".into(),
            repository_path: repository.to_string_lossy().into_owned(),
            objective: "Do not bypass unresolved authority".into(),
            acceptance_criteria: vec!["Unsafe handoffs are rejected".into()],
            at_millis: 1,
        })
        .unwrap();
    runtime
        .configure_mission(ConfigureMission {
            mission_id: "mission-guarded-handoff".into(),
            declarations: vec![command_check().covers(MissionDefinition::criterion_ids(&[
                "Unsafe handoffs are rejected".into(),
            ]))],
            at_millis: 2,
        })
        .unwrap();
    let source = runtime
        .start_run(StartRun {
            mission_id: "mission-guarded-handoff".into(),
            run_id: "run-source".into(),
            provider: ProviderKind::Codex,
            mode: ProviderMode::Managed,
            worktree_path: repository.to_string_lossy().into_owned(),
            request_id: ClaimRequestId::new("claim-source").unwrap(),
            execute_declared_checks: false,
            execute_project_recipe: false,
            at_millis: 3,
        })
        .unwrap();
    runtime
        .bind_provider_session("mission-guarded-handoff", "run-source", "session-source", 4)
        .unwrap();
    runtime
        .transition_run("mission-guarded-handoff", MissionStatus::Blocked, 5)
        .unwrap();
    runtime.release_worktree(&source.lease).unwrap();

    let same_provider = runtime.continue_run(ContinueRun {
        mission_id: "mission-guarded-handoff".into(),
        source_run_id: "run-source".into(),
        run_id: "run-next".into(),
        provider: ProviderKind::Codex,
        mode: ProviderMode::Managed,
        request_id: ClaimRequestId::new("claim-same-provider").unwrap(),
        handoff_artifact_sha256: "c".repeat(64),
        at_millis: 6,
    });
    assert!(matches!(
        same_provider,
        Err(MissionRuntimeError::Handoff(
            super::handoff::MissionHandoffError::SameProvider
        ))
    ));

    runtime
        .commit(
            "handoff-attention",
            PersistableMissionEvent::AttentionChanged {
                mission_id: "mission-guarded-handoff".into(),
                attention_id: "attention-open".into(),
                state: PersistedAttentionState::Open,
                risk: AttentionRisk::Critical,
                at_millis: 7,
            },
        )
        .unwrap();
    let unresolved = runtime.continue_run(ContinueRun {
        mission_id: "mission-guarded-handoff".into(),
        source_run_id: "run-source".into(),
        run_id: "run-claude".into(),
        provider: ProviderKind::ClaudeCode,
        mode: ProviderMode::Managed,
        request_id: ClaimRequestId::new("claim-unresolved").unwrap(),
        handoff_artifact_sha256: "d".repeat(64),
        at_millis: 8,
    });
    assert!(matches!(
        unresolved,
        Err(MissionRuntimeError::Handoff(
            super::handoff::MissionHandoffError::UnresolvedAttention
        ))
    ));
}

#[cfg(unix)]
#[test]
fn declared_checks_finalize_a_durable_ready_proof_pack() {
    use std::{os::unix::fs::PermissionsExt as _, process::Command, time::Duration};

    use super::executor::{execute_closure, ClosureExecutionRequest};

    fn git(repository: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repository)
            .output()
            .unwrap();
        assert!(output.status.success(), "git command failed: {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    let session = tempfile::tempdir().unwrap();
    let repository = tempfile::tempdir().unwrap();
    git(repository.path(), &["init", "-q"]);
    git(repository.path(), &["config", "user.name", "Nagi Test"]);
    git(
        repository.path(),
        &["config", "user.email", "nagi@example.invalid"],
    );
    let script = repository.path().join("verify");
    std::fs::write(&script, "#!/bin/sh\nprintf 'proof-ready'\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(repository.path(), &["add", "verify"]);
    git(repository.path(), &["commit", "-qm", "fixture"]);
    let repository = repository.path().canonicalize().unwrap();

    let claims = session.path().join("claims");
    let mut runtime = MissionRuntime::open_owned(session.path(), &claims).unwrap();
    runtime
        .create_mission(CreateMission {
            mission_id: "mission-proof".into(),
            title: "Prove closure".into(),
            repository_path: repository.to_string_lossy().into_owned(),
            objective: "Close only after the declared check passes".into(),
            acceptance_criteria: vec!["The verifier exits successfully".into()],
            at_millis: 1,
        })
        .unwrap();
    let criterion_ids =
        MissionDefinition::criterion_ids(&["The verifier exits successfully".to_owned()]);
    runtime
        .configure_mission(ConfigureMission {
            mission_id: "mission-proof".into(),
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
    let started = runtime
        .start_run(StartRun {
            mission_id: "mission-proof".into(),
            run_id: "run-proof".into(),
            provider: ProviderKind::Codex,
            mode: ProviderMode::Managed,
            worktree_path: repository.to_string_lossy().into_owned(),
            request_id: ClaimRequestId::new("run-proof").unwrap(),
            execute_declared_checks: true,
            execute_project_recipe: false,
            at_millis: 3,
        })
        .unwrap();
    runtime
        .bind_provider_session("mission-proof", "run-proof", "session-proof", 4)
        .unwrap();
    runtime
        .transition_run("mission-proof", MissionStatus::ReviewRequired, 5)
        .unwrap();
    let mission = runtime.mission("mission-proof").unwrap();
    let run = mission.run.as_ref().unwrap();
    let pack = execute_closure(ClosureExecutionRequest {
        mission_id: mission.mission_id.clone(),
        run_id: run.run_id.clone(),
        repository_path: mission.repository_path.clone(),
        worktree_path: run.worktree_path.clone(),
        base_revision: run.base_revision.clone(),
        declarations: mission.check_declarations.clone(),
    })
    .unwrap();
    let finalized_at = pack.created_at_millis().saturating_add(1);
    let outcome = runtime
        .finalize_evidence_pack(pack, &started.lease, finalized_at)
        .unwrap();

    assert!(outcome.verified);
    assert_eq!(outcome.mission.status, MissionStatus::ReadyToClose);
    assert_eq!(
        outcome.mission.latest_evidence_pack_digest.as_deref(),
        Some(outcome.pack_digest.as_str())
    );
    let ready_pack_digest = outcome.pack_digest;

    let proof_outcome = crate::server::mission_bridge::handle(
        &mut runtime,
        "proof-request",
        &crate::api::schema::Method::MissionProofGet(crate::api::schema::MissionTarget {
            mission_id: "mission-proof".into(),
        }),
    )
    .unwrap();
    assert!(!proof_outcome.changed);
    let proof_response: crate::api::schema::SuccessResponse =
        serde_json::from_str(&proof_outcome.response).unwrap();
    let crate::api::schema::ResponseResult::MissionProof { receipt } = proof_response.result else {
        panic!("expected a portable mission proof")
    };
    assert_eq!(receipt.identity.mission_id, "mission-proof");
    assert_eq!(receipt.identity.run_id, "run-proof");
    assert_eq!(receipt.seal_digest.len(), 64);
    assert_eq!(receipt.fresh_evidence.len(), 1);
    assert_eq!(receipt.fresh_evidence[0].check_id, "verify");
    assert_eq!(
        receipt.decision,
        crate::api::schema::ProofClosureDecisionV1::ReadyToClose
    );

    std::thread::sleep(Duration::from_millis(2));
    let mission = runtime.mission("mission-proof").unwrap();
    let run = mission.run.as_ref().unwrap();
    let archive_pack = execute_closure(ClosureExecutionRequest {
        mission_id: mission.mission_id.clone(),
        run_id: run.run_id.clone(),
        repository_path: mission.repository_path.clone(),
        worktree_path: run.worktree_path.clone(),
        base_revision: run.base_revision.clone(),
        declarations: mission.check_declarations.clone(),
    })
    .unwrap();
    let archive_at = archive_pack
        .created_at_millis()
        .saturating_add(1)
        .max(finalized_at.saturating_add(1));
    let archive_outcome = runtime
        .finalize_evidence_pack(archive_pack, &started.lease, archive_at)
        .unwrap();
    assert!(archive_outcome.verified);
    assert_eq!(archive_outcome.mission.status, MissionStatus::Archived);
    assert_ne!(archive_outcome.pack_digest, ready_pack_digest);
    let archive_pack_digest = archive_outcome.pack_digest;

    let archived_proof = crate::server::mission_bridge::handle(
        &mut runtime,
        "archived-proof-request",
        &crate::api::schema::Method::MissionProofGet(crate::api::schema::MissionTarget {
            mission_id: "mission-proof".into(),
        }),
    )
    .unwrap();
    let archived_response: crate::api::schema::SuccessResponse =
        serde_json::from_str(&archived_proof.response).unwrap();
    let crate::api::schema::ResponseResult::MissionProof { receipt } = archived_response.result
    else {
        panic!("expected an archived portable mission proof")
    };
    assert_eq!(
        receipt.decision,
        crate::api::schema::ProofClosureDecisionV1::Archived
    );

    runtime.release_worktree(&started.lease).unwrap();
    drop(runtime);

    let restored = MissionRuntime::open_owned(session.path(), &claims).unwrap();
    assert_eq!(
        restored.mission("mission-proof").unwrap().status,
        MissionStatus::Archived
    );
    let restored_pack = restored.load_evidence_pack(&archive_pack_digest).unwrap();
    assert_eq!(restored_pack.mission_id(), "mission-proof");
    assert_eq!(restored_pack.run_id(), "run-proof");
}

#[test]
fn durable_run_and_provider_session_survive_restart() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    store
        .commit(
            "event-2",
            PersistableMissionEvent::closure_configured(
                "mission-1",
                vec![CheckDeclaration::command(
                    "test",
                    CommandSpec::new("cargo", ["test"], "."),
                    vec![PathRule::All],
                    Vec::new(),
                )
                .covers(MissionDefinition::criterion_ids(&[
                    "The redirect test passes".to_owned(),
                ]))],
                15,
            )
            .unwrap(),
        )
        .unwrap();
    store
        .commit(
            "event-3",
            PersistableMissionEvent::run_started(
                "mission-1",
                "run-1",
                ProviderKind::Codex,
                ProviderMode::Managed,
                "/repo",
                "a".repeat(40),
                20,
            )
            .unwrap(),
        )
        .unwrap();
    store
        .commit(
            "event-4",
            PersistableMissionEvent::provider_session_bound("mission-1", "run-1", "thread-1", 30)
                .unwrap(),
        )
        .unwrap();
    drop(store);

    let restored = MissionStore::open(directory.path()).unwrap();
    let mission = restored.projection().mission_view("mission-1").unwrap();
    let run = mission.run.unwrap();
    assert_eq!(mission.status, MissionStatus::Active);
    assert_eq!(run.run_id, "run-1");
    assert_eq!(run.provider, ProviderKind::Codex);
    assert_eq!(run.mode, ProviderMode::Managed);
    assert_eq!(run.provider_session_id.as_deref(), Some("thread-1"));
    assert_eq!(restored.last_sequence(), 4);
}

#[test]
fn persisted_active_managed_run_reacquires_a_fresh_lease_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let claims = directory.path().join("claims");
    let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let mut runtime = MissionRuntime::open_owned(directory.path(), &claims).unwrap();
    runtime
        .create_mission(CreateMission {
            mission_id: "mission-recovery".into(),
            title: "Recover safely".into(),
            repository_path: repository.to_string_lossy().into_owned(),
            objective: "Resume the existing provider session".into(),
            acceptance_criteria: vec!["No duplicate turn is sent".into()],
            at_millis: 10,
        })
        .unwrap();
    configure_runtime_closure(&mut runtime, "mission-recovery", 15);
    let started = runtime
        .start_run(StartRun {
            mission_id: "mission-recovery".into(),
            run_id: "run-recovery".into(),
            provider: ProviderKind::Codex,
            mode: ProviderMode::Managed,
            worktree_path: repository.to_string_lossy().into_owned(),
            request_id: ClaimRequestId::new("initial-recovery-claim").unwrap(),
            execute_declared_checks: false,
            execute_project_recipe: false,
            at_millis: 20,
        })
        .unwrap();
    runtime
        .bind_provider_session("mission-recovery", "run-recovery", "thread-recovery", 30)
        .unwrap();
    runtime.release_worktree(&started.lease).unwrap();
    drop(runtime);

    let recovered = MissionRuntime::open_owned(directory.path(), &claims).unwrap();
    let outcome = recovered
        .recover_managed_run(
            "mission-recovery",
            ClaimRequestId::new("restart-recovery-claim").unwrap(),
        )
        .unwrap();

    assert_eq!(outcome.mission.status, MissionStatus::Active);
    assert_eq!(
        outcome
            .mission
            .run
            .as_ref()
            .and_then(|run| run.provider_session_id.as_deref()),
        Some("thread-recovery")
    );
    assert!(recovered.release_worktree(&outcome.lease).is_ok());
}

fn configure_runtime_closure(runtime: &mut MissionRuntime, mission_id: &str, at_millis: u64) {
    let mission = runtime.mission(mission_id).unwrap();
    let criterion_ids = MissionDefinition::criterion_ids(&mission.acceptance_criteria);
    runtime
        .configure_mission(ConfigureMission {
            mission_id: mission_id.to_owned(),
            declarations: vec![CheckDeclaration::command(
                "test",
                CommandSpec::new("cargo", ["test"], "."),
                vec![PathRule::All],
                Vec::new(),
            )
            .covers(criterion_ids)],
            at_millis,
        })
        .unwrap();
}

fn applied_attention<T>(
    mutation: (AttentionInbox, Result<T, AttentionError>),
) -> (AttentionInbox, T) {
    let (inbox, result) = mutation;
    (inbox, result.unwrap())
}

fn attention_with(event: AttentionEvent) -> AttentionInbox {
    AttentionInbox::new().ingest(event).unwrap()
}

fn mission_definition(id: &str) -> MissionDefinition {
    MissionDefinition::new(
        id,
        "Fix login redirect",
        "/repo",
        "Users land on the requested page after login",
        [
            "The redirect test passes",
            "The login flow has no regression",
        ],
    )
    .unwrap()
}

fn closure_check(mission_id: &str) -> CheckDeclaration {
    let definition = mission_definition(mission_id);
    command_check().covers(definition.acceptance_criterion_ids())
}

fn run_target(worktree: &str) -> RunTarget {
    RunTarget::new(
        "run-1",
        ProviderKind::Codex,
        ProviderMode::Managed,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        worktree,
    )
    .unwrap()
}

fn structured(
    event_id: &str,
    status: SessionStatus,
    provider_sequence: u64,
    received_sequence: u64,
    turn_id: &str,
    observed_at_millis: u64,
) -> SessionObservation {
    SessionObservation::new(
        event_id,
        status,
        ObservationSource::StructuredHook,
        Some(provider_sequence),
        received_sequence,
        Some(turn_id),
        observed_at_millis,
    )
}

fn permission(
    event_id: &str,
    request_id: &str,
    scope: &str,
    risk: AttentionRisk,
) -> AttentionEvent {
    AttentionEvent::new(
        event_id,
        "mission-1",
        "run-1",
        "session-1",
        PaneTarget::new("workspace-1", "terminal-7"),
        AttentionKind::PermissionRequest,
        "Run database migration",
        scope,
        risk,
        ProviderKind::Codex,
        ObservationSource::StructuredHook,
        10,
    )
    .with_provider_request_id(request_id)
    .with_response_capability(ResponseCapability::Reliable)
}

fn workspace(hash: &str) -> WorkspaceSnapshot {
    WorkspaceSnapshot::new(
        format!("tree-{hash}"),
        format!("diff-{hash}"),
        vec![FileFingerprint::new(
            "src/lib.rs",
            hash,
            FileDisposition::Tracked,
        )],
    )
    .with_artifacts([("target/report.json", "artifact-hash")])
}

fn proof_identity(mission_id: &str) -> ProofIdentity {
    ProofIdentity::new(
        mission_id,
        "run-1",
        "/repo",
        "/repo/.worktrees/mission-1",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .unwrap()
}

fn evaluate_test_proof(
    lifecycle: &MissionLifecycle,
    declarations: &[CheckDeclaration],
    records: &BTreeMap<String, EvidenceRecord>,
    current: &WorkspaceSnapshot,
    unresolved_attention_ids: &std::collections::BTreeSet<String>,
    authority_sequence: u64,
    at_millis: u64,
) -> Result<ProofReport, MissionError> {
    let identity = lifecycle.proof_identity()?;
    let authority = AuthoritySnapshot::for_test(
        &identity,
        current,
        records,
        unresolved_attention_ids,
        authority_sequence,
        at_millis,
    );
    lifecycle.evaluate_proof(
        declarations,
        records,
        current,
        unresolved_attention_ids,
        &authority,
    )
}

fn ready_proof_for(mission_id: &str, worktree: &str, at_millis: u64) -> ReadyProof {
    ready_proof_at(mission_id, worktree, at_millis.max(1), at_millis)
}

fn ready_proof_at(
    mission_id: &str,
    worktree: &str,
    authority_sequence: u64,
    at_millis: u64,
) -> ReadyProof {
    ready_proof_at_head(mission_id, worktree, authority_sequence, at_millis, None)
}

fn ready_proof_at_head(
    mission_id: &str,
    worktree: &str,
    authority_sequence: u64,
    at_millis: u64,
    head_digest: Option<&str>,
) -> ReadyProof {
    let definition = mission_definition(mission_id);
    let criteria = definition.acceptance_criterion_ids();
    let identity = ProofIdentity::new(
        mission_id,
        "run-1",
        "/repo",
        worktree,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .unwrap();
    let check = command_check().covers(criteria.clone());
    let lifecycle = MissionLifecycle::draft(definition, 0)
        .with_run_target(
            RunTarget::new(
                "run-1",
                ProviderKind::Codex,
                ProviderMode::Managed,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                worktree,
            )
            .unwrap(),
            std::slice::from_ref(&check),
            "test",
            1,
        )
        .unwrap()
        .transition(MissionStatus::Active, "runtime", "provider ready", 2)
        .unwrap()
        .transition(
            MissionStatus::ReviewRequired,
            "runtime",
            "turn completed",
            3,
        )
        .unwrap();
    let state = workspace("proof");
    let evidence = CommandEvidence::new(
        &check,
        &identity,
        &state,
        &state,
        0,
        at_millis.saturating_sub(10),
        at_millis,
        vec![ArtifactEvidence::new(
            "target/report.json",
            "artifact-hash",
            "application/json",
        )],
    )
    .unwrap();
    let records = BTreeMap::from([(
        check.id().to_owned(),
        EvidenceRecord::Command(Box::new(evidence)),
    )]);

    let blockers = std::collections::BTreeSet::new();
    let authority = match head_digest {
        Some(head_digest) => AuthoritySnapshot::for_test_at_head(
            &identity,
            &state,
            &records,
            &blockers,
            authority_sequence,
            at_millis,
            head_digest,
        ),
        None => AuthoritySnapshot::for_test(
            &identity,
            &state,
            &records,
            &blockers,
            authority_sequence,
            at_millis,
        ),
    };
    lifecycle
        .evaluate_ready_proof(&[check], &records, &state, &blockers, &authority)
        .unwrap()
}

fn archive_proof_at(
    lifecycle: &MissionLifecycle,
    authority_sequence: u64,
    at_millis: u64,
) -> Result<ArchiveProof, MissionError> {
    archive_proof_at_head(lifecycle, authority_sequence, at_millis, None)
}

fn archive_proof_at_head(
    lifecycle: &MissionLifecycle,
    authority_sequence: u64,
    at_millis: u64,
    head_digest: Option<&str>,
) -> Result<ArchiveProof, MissionError> {
    let identity = lifecycle.proof_identity()?;
    let check = closure_check(identity.mission_id());
    let state = workspace("proof");
    let evidence = CommandEvidence::new(
        &check,
        &identity,
        &state,
        &state,
        0,
        at_millis.saturating_sub(10),
        at_millis,
        vec![ArtifactEvidence::new(
            "target/report.json",
            "artifact-hash",
            "application/json",
        )],
    )
    .unwrap();
    let records = BTreeMap::from([(
        check.id().to_owned(),
        EvidenceRecord::Command(Box::new(evidence)),
    )]);
    let blockers = std::collections::BTreeSet::new();
    let authority = match head_digest {
        Some(head_digest) => AuthoritySnapshot::for_test_at_head(
            &identity,
            &state,
            &records,
            &blockers,
            authority_sequence,
            at_millis,
            head_digest,
        ),
        None => AuthoritySnapshot::for_test(
            &identity,
            &state,
            &records,
            &blockers,
            authority_sequence,
            at_millis,
        ),
    };
    lifecycle.evaluate_archive_proof(&[check], &records, &state, &blockers, &authority)
}

fn command_check() -> CheckDeclaration {
    CheckDeclaration::command(
        "tests",
        CommandSpec::new("cargo", ["test"], "."),
        vec![PathRule::prefix("src/")],
        vec![ArtifactRequirement::new("target/report.json")],
    )
}

fn command_evidence(check: &CheckDeclaration, state: &WorkspaceSnapshot) -> CommandEvidence {
    CommandEvidence::new(
        check,
        &proof_identity("mission-1"),
        state,
        state,
        0,
        100,
        150,
        vec![ArtifactEvidence::new(
            "target/report.json",
            "artifact-hash",
            "application/json",
        )],
    )
    .unwrap()
}

#[test]
fn structured_state_has_exact_provenance() {
    let snapshot = SessionSnapshot::starting(0).apply(structured(
        "turn-started",
        SessionStatus::Working,
        1,
        1,
        "turn-1",
        10,
    ));

    assert_eq!(snapshot.status(), SessionStatus::Working);
    assert_eq!(snapshot.source(), ObservationSource::StructuredHook);
    assert_eq!(snapshot.confidence(), Confidence::Exact);
}

#[test]
fn terminal_heuristic_cannot_claim_working_or_completion() {
    let snapshot = SessionSnapshot::starting(0);
    let working = snapshot.clone().apply(SessionObservation::new(
        "screen-working",
        SessionStatus::Working,
        ObservationSource::TerminalHeuristic,
        None,
        1,
        None,
        10,
    ));
    let completed = snapshot.apply(
        SessionObservation::new(
            "screen-completed",
            SessionStatus::Ready,
            ObservationSource::TerminalHeuristic,
            None,
            2,
            Some("turn-1"),
            20,
        )
        .with_turn_completed(),
    );

    assert_eq!(working.status(), SessionStatus::Unknown);
    assert_eq!(completed.status(), SessionStatus::Unknown);
    assert!(!completed.current_turn_completed());
}

#[test]
fn duplicate_and_late_events_cannot_regress_a_completed_turn() {
    let completed_event = structured("turn-completed", SessionStatus::Ready, 5, 5, "turn-1", 50)
        .with_turn_completed();
    let completed = SessionSnapshot::starting(0)
        .apply(completed_event.clone())
        .apply(completed_event);
    let updated = completed.apply(structured(
        "late-working",
        SessionStatus::Working,
        4,
        6,
        "turn-1",
        60,
    ));

    assert_eq!(updated.status(), SessionStatus::Ready);
    assert!(updated.current_turn_completed());
    assert_eq!(updated.applied_event_count(), 1);
}

#[test]
fn equal_sequences_cannot_overwrite_an_applied_observation() {
    let working = SessionSnapshot::starting(0).apply(structured(
        "working",
        SessionStatus::Working,
        5,
        5,
        "turn-1",
        50,
    ));

    let equal_provider_sequence = working.clone().apply(structured(
        "conflicting-provider-event",
        SessionStatus::Failed,
        5,
        6,
        "turn-1",
        60,
    ));
    let equal_received_sequence = working.apply(structured(
        "conflicting-local-event",
        SessionStatus::Failed,
        6,
        5,
        "turn-1",
        60,
    ));

    assert_eq!(equal_provider_sequence.status(), SessionStatus::Working);
    assert_eq!(equal_provider_sequence.applied_event_count(), 1);
    assert_eq!(equal_received_sequence.status(), SessionStatus::Working);
    assert_eq!(equal_received_sequence.applied_event_count(), 1);
}

#[test]
fn an_unscoped_disconnect_preserves_the_current_turn_identity() {
    let completed = SessionSnapshot::starting(0).apply(
        structured("complete", SessionStatus::Ready, 5, 5, "turn-1", 50).with_turn_completed(),
    );
    let disconnected = completed.apply(SessionObservation::new(
        "process-disconnected",
        SessionStatus::Disconnected,
        ObservationSource::Process,
        None,
        6,
        None,
        60,
    ));

    assert_eq!(disconnected.status(), SessionStatus::Disconnected);
    assert_eq!(disconnected.current_turn_id(), Some("turn-1"));
    assert!(disconnected.current_turn_completed());
}

#[test]
fn a_late_structured_failure_can_override_a_completed_turn() {
    let completed = SessionSnapshot::starting(0).apply(
        structured("complete", SessionStatus::Ready, 5, 5, "turn-1", 50).with_turn_completed(),
    );
    let failed = completed.apply(structured(
        "late-failure",
        SessionStatus::Failed,
        6,
        6,
        "turn-1",
        60,
    ));

    assert_eq!(failed.status(), SessionStatus::Failed);
    assert_eq!(failed.current_turn_id(), Some("turn-1"));
    assert!(failed.current_turn_completed());
}

#[test]
fn a_terminal_heuristic_cannot_overwrite_an_exact_turn_state() {
    let working = SessionSnapshot::starting(0).apply(structured(
        "working",
        SessionStatus::Working,
        5,
        5,
        "turn-1",
        50,
    ));
    let inferred = working.apply(SessionObservation::new(
        "screen-idle",
        SessionStatus::Ready,
        ObservationSource::TerminalHeuristic,
        None,
        6,
        Some("invented-turn"),
        60,
    ));

    assert_eq!(inferred.status(), SessionStatus::Working);
    assert_eq!(inferred.source(), ObservationSource::StructuredHook);
    assert_eq!(inferred.current_turn_id(), Some("turn-1"));
    assert_eq!(inferred.applied_event_count(), 1);
}

#[test]
fn stale_observation_is_visible_as_unknown() {
    let snapshot = SessionSnapshot::starting(0)
        .apply(structured("working", SessionStatus::Working, 1, 1, "turn-1", 100).expires_at(150));

    let visible = snapshot.visible_at(151);
    assert_eq!(visible.status, SessionStatus::Unknown);
    assert_eq!(visible.age_millis, 51);
}

#[test]
fn semantic_attention_deduplicates_only_equal_action_scope_and_risk() {
    let inbox = AttentionInbox::new()
        .ingest(permission(
            "event-1",
            "request-1",
            "worktree-a",
            AttentionRisk::High,
        ))
        .unwrap()
        .ingest(permission(
            "event-2",
            "request-1",
            "worktree-a",
            AttentionRisk::High,
        ))
        .unwrap()
        .ingest(permission(
            "event-3",
            "request-2",
            "worktree-b",
            AttentionRisk::High,
        ))
        .unwrap();

    assert_eq!(inbox.len(), 2);
    assert_eq!(inbox.items().next().unwrap().occurrence_count(), 2);
}

#[test]
fn expired_permission_cannot_be_approved() {
    let inbox = attention_with(
        permission("event-1", "request-1", "worktree-a", AttentionRisk::High).expires_at(20),
    )
    .refresh(21);
    let item_id = inbox.items().next().unwrap().id().to_owned();

    let (_, result) = inbox.decide(&item_id, AttentionDecision::ApproveOnce, "user", 21);
    let error = result.unwrap_err();

    assert_eq!(error.to_string(), "attention item has expired");
    assert_eq!(
        inbox.item(&item_id).unwrap().status(),
        &AttentionStatus::Expired { at_millis: 21 }
    );
}

#[test]
fn unreliable_reply_opens_the_exact_native_pane_without_resolving() {
    let inbox = attention_with(
        permission("event-1", "request-1", "worktree-a", AttentionRisk::High)
            .with_response_capability(ResponseCapability::OpenPaneOnly),
    );
    let item_id = inbox.items().next().unwrap().id().to_owned();

    let (unchanged, response) =
        applied_attention(inbox.decide(&item_id, AttentionDecision::ApproveOnce, "user", 20));

    assert_eq!(unchanged, inbox);
    assert_eq!(
        response,
        Some(ProviderResponseIntent::OpenPane {
            target: PaneTarget::new("workspace-1", "terminal-7"),
        })
    );
}

#[test]
fn provider_decision_stays_pending_until_the_provider_acknowledges_it() {
    let inbox = attention_with(permission(
        "event-1",
        "request-1",
        "worktree-a",
        AttentionRisk::High,
    ));
    let item_id = inbox.items().next().unwrap().id().to_owned();

    let (pending, response) =
        applied_attention(inbox.decide(&item_id, AttentionDecision::ApproveOnce, "user", 20));

    let Some(ProviderResponseIntent::Respond {
        route,
        decision: AttentionDecision::ApproveOnce,
        answer: None,
        token,
    }) = response
    else {
        panic!("expected a routed provider response");
    };
    assert_eq!(route.provider(), ProviderKind::Codex);
    assert_eq!(route.mission_id(), "mission-1");
    assert_eq!(route.mission_run_id(), "run-1");
    assert_eq!(route.session_id(), "session-1");
    assert_eq!(route.provider_request_id(), "request-1");
    assert_eq!(route.scope(), "worktree-a");
    assert!(matches!(
        pending.item(&item_id).unwrap().status(),
        AttentionStatus::PendingResponse {
            decision: AttentionDecision::ApproveOnce,
            actor,
            requested_at_millis: 20,
        } if actor == "user"
    ));
    assert_eq!(
        pending.effective_run_status(
            "mission-1",
            "run-1",
            ProviderKind::Codex,
            "session-1",
            SessionStatus::Working,
        ),
        SessionStatus::NeedsApproval
    );

    let (resolved, ()) = applied_attention(pending.confirm_response(&token, 30));
    assert!(matches!(
        resolved.item(&item_id).unwrap().status(),
        AttentionStatus::Resolved {
            decision: AttentionDecision::ApproveOnce,
            actor,
            at_millis: 30,
        } if actor == "user"
    ));
}

#[test]
fn provider_question_answer_is_ephemeral_and_redacted_from_durable_state() {
    let secret_answer = "Use the blue deployment target";
    let question = AttentionEvent::new(
        "question-event-1",
        "mission-1",
        "run-1",
        "session-1",
        PaneTarget::new("workspace-1", "terminal-7"),
        AttentionKind::ProviderQuestion,
        "Which deployment target should I use?",
        "worktree-a",
        AttentionRisk::Low,
        ProviderKind::Codex,
        ObservationSource::ProviderApi,
        10,
    )
    .with_provider_request_id("question-request-1")
    .with_response_capability(ResponseCapability::Reliable);
    let inbox = attention_with(question);
    let item_id = inbox.items().next().unwrap().id().to_owned();

    let (pending, response) =
        applied_attention(inbox.answer(&item_id, secret_answer, "local-user", 20));
    let Some(ProviderResponseIntent::Respond {
        decision: AttentionDecision::Answer,
        answer: Some(answer),
        ..
    }) = response
    else {
        panic!("expected an ephemeral provider answer");
    };

    assert_eq!(answer.expose_to_provider(), secret_answer);
    assert!(!format!("{answer:?}").contains(secret_answer));
    assert!(!serde_json::to_string(&pending)
        .unwrap()
        .contains(secret_answer));
    assert!(matches!(
        pending.item(&item_id).unwrap().status(),
        AttentionStatus::PendingResponse {
            decision: AttentionDecision::Answer,
            ..
        }
    ));
    assert_eq!(
        inbox.answer(&item_id, "", "local-user", 21).1.unwrap_err(),
        AttentionError::EmptyAnswer
    );
}

#[test]
fn provider_ack_after_dispatch_can_resolve_past_the_request_deadline() {
    let inbox = attention_with(
        permission("event-1", "request-1", "worktree-a", AttentionRisk::Low).expires_at(30),
    );
    let item_id = inbox.items().next().unwrap().id().to_owned();
    let (pending, response) =
        applied_attention(inbox.decide(&item_id, AttentionDecision::ApproveOnce, "user", 20));
    let ProviderResponseIntent::Respond { token, .. } = response.unwrap() else {
        panic!("reliable permission should create a provider response");
    };

    let (resolved, ()) = applied_attention(pending.confirm_response(&token, 30));
    assert!(matches!(
        resolved.item(&item_id).unwrap().status(),
        AttentionStatus::Resolved { at_millis: 30, .. }
    ));
}

#[test]
fn an_expired_in_flight_response_requires_reconciliation() {
    let inbox = attention_with(
        permission("event-1", "request-1", "worktree-a", AttentionRisk::Low).expires_at(30),
    );
    let item_id = inbox.items().next().unwrap().id().to_owned();
    let (pending, response) =
        applied_attention(inbox.decide(&item_id, AttentionDecision::ApproveOnce, "user", 20));
    assert!(matches!(
        response,
        Some(ProviderResponseIntent::Respond { .. })
    ));

    let expired = pending.refresh(30);

    assert!(matches!(
        expired.item(&item_id).unwrap().status(),
        AttentionStatus::ReconciliationRequired {
            code: ResponseFailureCode::Timeout,
            at_millis: 30,
            ..
        }
    ));
}

#[test]
fn failed_provider_response_reopens_the_item_and_keeps_the_attempt_audit() {
    let inbox = attention_with(permission(
        "event-1",
        "request-1",
        "worktree-a",
        AttentionRisk::High,
    ));
    let item_id = inbox.items().next().unwrap().id().to_owned();
    let (pending, response) =
        applied_attention(inbox.decide(&item_id, AttentionDecision::Deny, "user", 20));
    let ProviderResponseIntent::Respond { token, .. } = response.unwrap() else {
        panic!("expected response token");
    };

    let (reopened, ()) = applied_attention(pending.fail_response(
        &token,
        ResponseFailureDisposition::DefinitelyNotApplied,
        ResponseFailureCode::DisconnectedBeforeWrite,
        30,
    ));
    let item = reopened.item(&item_id).unwrap();

    assert_eq!(item.status(), &AttentionStatus::Open);
    assert_eq!(item.response_attempts().len(), 1);
    assert_eq!(
        item.response_attempts()[0].failure_disposition(),
        Some(ResponseFailureDisposition::DefinitelyNotApplied)
    );
    assert_eq!(
        item.response_attempts()[0].failure_code(),
        Some(ResponseFailureCode::DisconnectedBeforeWrite)
    );
    assert_eq!(reopened.unread_count(), 1);
}

#[test]
fn unknown_response_delivery_stays_blocked_until_reconciled() {
    let inbox = attention_with(permission(
        "event-1",
        "request-1",
        "worktree-a",
        AttentionRisk::High,
    ));
    let item_id = inbox.items().next().unwrap().id().to_owned();
    let (pending, response) =
        applied_attention(inbox.decide(&item_id, AttentionDecision::Deny, "user", 20));
    let ProviderResponseIntent::Respond { token, .. } = response.unwrap() else {
        panic!("expected response token");
    };
    let (reconciliation, ()) = applied_attention(pending.fail_response(
        &token,
        ResponseFailureDisposition::DeliveryUnknown,
        ResponseFailureCode::Timeout,
        30,
    ));

    assert!(matches!(
        reconciliation.item(&item_id).unwrap().status(),
        AttentionStatus::ReconciliationRequired {
            decision: AttentionDecision::Deny,
            actor,
            code: ResponseFailureCode::Timeout,
            at_millis: 30,
        } if actor == "user"
    ));
    let (unchanged, retry) = reconciliation.decide(&item_id, AttentionDecision::Deny, "user", 40);
    assert_eq!(retry.unwrap_err(), AttentionError::AlreadyClosed);
    assert_eq!(unchanged, reconciliation);

    let (resolved, ()) = applied_attention(reconciliation.confirm_response(&token, 50));
    assert!(matches!(
        resolved.item(&item_id).unwrap().status(),
        AttentionStatus::Resolved { at_millis: 50, .. }
    ));
}

fn response_token(inbox: &AttentionInbox, item_id: &str) -> Option<ProviderResponseToken> {
    inbox
        .item(item_id)
        .and_then(|item| item.response_attempts().last())
        .map(|attempt| attempt.token().clone())
}

#[test]
fn permission_expiry_is_checked_atomically_without_refresh() {
    let inbox = attention_with(
        permission("event-1", "request-1", "worktree-a", AttentionRisk::High).expires_at(20),
    );
    let item_id = inbox.items().next().unwrap().id().to_owned();

    let (expired, result) = inbox.decide(&item_id, AttentionDecision::ApproveOnce, "user", 20);
    let error = result.unwrap_err();

    assert_eq!(error.to_string(), "attention item has expired");
    assert_eq!(
        expired.item(&item_id).unwrap().status(),
        &AttentionStatus::Expired { at_millis: 20 }
    );
}

#[test]
fn distinct_provider_requests_with_equal_copy_never_merge() {
    let inbox = AttentionInbox::new()
        .ingest(permission(
            "event-1",
            "request-1",
            "worktree-a",
            AttentionRisk::High,
        ))
        .unwrap()
        .ingest(
            permission("event-2", "request-2", "worktree-a", AttentionRisk::High)
                .with_request_generation(2),
        )
        .unwrap();

    assert_eq!(inbox.len(), 2);
}

#[test]
fn one_provider_request_identity_cannot_create_conflicting_actionable_items() {
    let inbox = attention_with(permission(
        "event-1",
        "request-1",
        "worktree-a",
        AttentionRisk::Low,
    ));
    let error = inbox
        .clone()
        .ingest(permission(
            "event-2",
            "request-1",
            "worktree-b",
            AttentionRisk::Critical,
        ))
        .unwrap_err();

    assert_eq!(inbox.len(), 1);
    assert_eq!(error, AttentionError::ProviderRequestConflict);
}

#[test]
fn critical_permission_cannot_be_allowed_for_the_whole_mission() {
    let inbox = attention_with(permission(
        "event-1",
        "request-1",
        "worktree-a",
        AttentionRisk::Critical,
    ));
    let item_id = inbox.items().next().unwrap().id().to_owned();

    let (unchanged, response) =
        applied_attention(inbox.decide(&item_id, AttentionDecision::AllowForMission, "user", 20));

    assert_eq!(unchanged, inbox);
    assert!(matches!(
        response,
        Some(ProviderResponseIntent::OpenPane { .. })
    ));
}

#[test]
fn heuristic_observation_can_never_gain_a_reliable_response_channel() {
    let event = AttentionEvent::new(
        "event-1",
        "mission-1",
        "run-1",
        "session-1",
        PaneTarget::new("workspace-1", "terminal-7"),
        AttentionKind::PermissionRequest,
        "Run database migration",
        "worktree-a",
        AttentionRisk::High,
        ProviderKind::Codex,
        ObservationSource::TerminalHeuristic,
        10,
    )
    .with_provider_request_id("request-1")
    .with_response_capability(ResponseCapability::Reliable);
    let inbox = attention_with(event);
    let item_id = inbox.items().next().unwrap().id().to_owned();

    let (unchanged, response) =
        applied_attention(inbox.decide(&item_id, AttentionDecision::ApproveOnce, "user", 20));

    assert_eq!(unchanged, inbox);
    assert!(matches!(
        response,
        Some(ProviderResponseIntent::OpenPane { .. })
    ));
}

#[test]
fn duplicate_provider_ack_is_idempotent_but_a_stale_token_cannot_close_a_retry() {
    let inbox = attention_with(permission(
        "event-1",
        "request-1",
        "worktree-a",
        AttentionRisk::High,
    ));
    let item_id = inbox.items().next().unwrap().id().to_owned();
    let (pending, response) =
        applied_attention(inbox.decide(&item_id, AttentionDecision::Deny, "user", 20));
    let ProviderResponseIntent::Respond {
        token: first_token, ..
    } = response.unwrap()
    else {
        panic!("expected response token");
    };
    let (reopened, ()) = applied_attention(pending.fail_response(
        &first_token,
        ResponseFailureDisposition::DefinitelyNotApplied,
        ResponseFailureCode::DisconnectedBeforeWrite,
        30,
    ));
    let (retry, response) =
        applied_attention(reopened.decide(&item_id, AttentionDecision::Deny, "user", 40));
    let ProviderResponseIntent::Respond {
        token: retry_token, ..
    } = response.unwrap()
    else {
        panic!("expected retry token");
    };

    let (unchanged, stale_result) = retry.confirm_response(&first_token, 50);
    assert_eq!(
        stale_result.unwrap_err().to_string(),
        "provider response token is stale"
    );
    assert_eq!(unchanged, retry);
    let (resolved, ()) = applied_attention(retry.confirm_response(&retry_token, 50));
    let (duplicate, duplicate_result) = resolved.confirm_response(&retry_token, 51);
    assert_eq!(duplicate_result, Ok(()));
    assert_eq!(duplicate, resolved);
    assert_eq!(response_token(&resolved, &item_id), Some(retry_token));
}

#[test]
fn attention_serializes_for_audit_without_becoming_recovery_authority() {
    let inbox = attention_with(permission(
        "event-1",
        "request-1",
        "worktree-a",
        AttentionRisk::High,
    ));

    let encoded = serde_json::to_value(&inbox).unwrap();

    assert_eq!(encoded["items"]["event-1"]["unread"], true);
    assert_eq!(inbox.unread_count(), 1);
}

#[test]
fn unresolved_approval_overlays_a_working_provider_state() {
    let inbox = attention_with(permission(
        "event-1",
        "request-1",
        "worktree-a",
        AttentionRisk::High,
    ));

    assert_eq!(
        inbox.effective_run_status(
            "mission-1",
            "run-1",
            ProviderKind::Codex,
            "session-1",
            SessionStatus::Working,
        ),
        SessionStatus::NeedsApproval
    );
}

#[test]
fn relevant_mutation_stales_command_evidence() {
    let check = command_check();
    let verified = workspace("a");
    let changed = workspace("b");
    let evidence = command_evidence(&check, &verified);

    assert_eq!(evidence.status_against(&changed), EvidenceStatus::Stale);
}

#[test]
fn path_prefix_matches_components_instead_of_textual_neighbors() {
    let check = CheckDeclaration::command(
        "component-prefix",
        CommandSpec::new("cargo", ["test"], "."),
        vec![PathRule::prefix("src")],
        Vec::new(),
    );
    let verified = WorkspaceSnapshot::new(
        "tree",
        "diff",
        vec![
            FileFingerprint::new("src/lib.rs", "a", FileDisposition::Tracked),
            FileFingerprint::new("src2/lib.rs", "b", FileDisposition::Tracked),
        ],
    );
    let changed_neighbor = WorkspaceSnapshot::new(
        "tree",
        "diff-2",
        vec![
            FileFingerprint::new("src/lib.rs", "a", FileDisposition::Tracked),
            FileFingerprint::new("src2/lib.rs", "changed", FileDisposition::Tracked),
        ],
    );
    let evidence = CommandEvidence::new(
        &check,
        &proof_identity("mission-1"),
        &verified,
        &verified,
        0,
        10,
        20,
        Vec::new(),
    )
    .unwrap();

    assert_eq!(
        evidence.status_against(&changed_neighbor),
        EvidenceStatus::Passed
    );

    let trailing_separator = CheckDeclaration::command(
        "component-prefix-trailing-separator",
        CommandSpec::new("cargo", ["test"], "."),
        vec![PathRule::prefix("src/")],
        Vec::new(),
    );
    let evidence = CommandEvidence::new(
        &trailing_separator,
        &proof_identity("mission-1"),
        &verified,
        &verified,
        0,
        10,
        20,
        Vec::new(),
    )
    .unwrap();
    let changed_child = WorkspaceSnapshot::new(
        "tree",
        "diff-3",
        vec![
            FileFingerprint::new("src/lib.rs", "changed", FileDisposition::Tracked),
            FileFingerprint::new("src2/lib.rs", "b", FileDisposition::Tracked),
        ],
    );
    assert_eq!(
        evidence.status_against(&changed_child),
        EvidenceStatus::Stale
    );
}

#[test]
fn path_contracts_are_canonicalized_before_they_are_persisted() {
    assert_eq!(PathRule::prefix("."), PathRule::All);
    assert_eq!(
        PathRule::prefix("./src//nested/"),
        PathRule::Prefix {
            prefix: "src/nested".to_owned(),
        }
    );
    assert_eq!(
        PathRule::exact("./src//lib.rs/"),
        PathRule::Exact {
            path: "src/lib.rs".to_owned(),
        }
    );

    let normalized = CheckDeclaration::command(
        "normalized",
        CommandSpec::new("cargo", ["test"], "./crates//core/"),
        vec![PathRule::prefix("./src//")],
        vec![ArtifactRequirement::new("./target//report.json/")],
    );
    let canonical = CheckDeclaration::command(
        "normalized",
        CommandSpec::new("cargo", ["test"], "crates/core"),
        vec![PathRule::prefix("src")],
        vec![ArtifactRequirement::new("target/report.json")],
    );

    assert_eq!(normalized, canonical);
    assert!(normalized.validate_persisted().is_ok());
}

#[test]
fn ambiguous_or_cross_platform_unsafe_paths_fail_closed() {
    for rule in [
        PathRule::exact("."),
        PathRule::exact("../src/lib.rs"),
        PathRule::prefix("/src"),
        PathRule::prefix("C:/src"),
        PathRule::prefix(r"C:\src"),
        PathRule::prefix(r"\\server\share"),
        PathRule::prefix(r"src\..\secret"),
    ] {
        let declaration = CheckDeclaration::command(
            "unsafe-rule",
            CommandSpec::new("cargo", ["test"], "."),
            vec![rule],
            Vec::new(),
        );
        assert!(declaration.validate_persisted().is_err());
    }

    for cwd in ["../", "/tmp", "C:/repo", r"C:\repo", r"\\server\repo"] {
        let declaration = CheckDeclaration::command(
            "unsafe-cwd",
            CommandSpec::new("cargo", ["test"], cwd),
            vec![PathRule::All],
            Vec::new(),
        );
        assert!(declaration.validate_persisted().is_err());
    }

    for artifact in [
        ".",
        "../report.json",
        "/tmp/report.json",
        "C:/report.json",
        r"C:\report.json",
    ] {
        let declaration = CheckDeclaration::command(
            "unsafe-artifact",
            CommandSpec::new("cargo", ["test"], "."),
            vec![PathRule::All],
            vec![ArtifactRequirement::new(artifact)],
        );
        assert!(declaration.validate_persisted().is_err());
    }

    let unsafe_evidence_check = CheckDeclaration::command(
        "unsafe-evidence",
        CommandSpec::new("cargo", ["test"], "."),
        vec![PathRule::exact(".")],
        Vec::new(),
    );
    let state = WorkspaceSnapshot::new(
        "tree",
        "diff",
        vec![FileFingerprint::new(
            "src/lib.rs",
            "hash",
            FileDisposition::Tracked,
        )],
    );
    assert!(CommandEvidence::new(
        &unsafe_evidence_check,
        &proof_identity("mission-1"),
        &state,
        &state,
        0,
        10,
        20,
        Vec::new(),
    )
    .is_err());
}

#[test]
fn root_prefix_covers_every_file_and_stales_after_any_mutation() {
    let check = CheckDeclaration::command(
        "root-prefix",
        CommandSpec::new("cargo", ["test"], "."),
        vec![PathRule::prefix(".")],
        Vec::new(),
    );
    let verified = WorkspaceSnapshot::new(
        "tree-a",
        "diff-a",
        vec![FileFingerprint::new(
            "src/lib.rs",
            "hash-a",
            FileDisposition::Tracked,
        )],
    );
    let changed = WorkspaceSnapshot::new(
        "tree-b",
        "diff-b",
        vec![FileFingerprint::new(
            "src/lib.rs",
            "hash-b",
            FileDisposition::Tracked,
        )],
    );
    let evidence = CommandEvidence::new(
        &check,
        &proof_identity("mission-1"),
        &verified,
        &verified,
        0,
        10,
        20,
        Vec::new(),
    )
    .unwrap();

    assert_eq!(evidence.status_against(&changed), EvidenceStatus::Stale);
}

#[test]
fn ignored_mutation_is_excluded_unless_explicitly_requested() {
    let verified = WorkspaceSnapshot::new(
        "tree-a",
        "diff-a",
        vec![FileFingerprint::new(
            "src/generated.cache",
            "hash-a",
            FileDisposition::Ignored,
        )],
    )
    .with_artifacts([("target/report.json", "artifact-hash")]);
    let changed = WorkspaceSnapshot::new(
        "tree-a",
        "diff-a",
        vec![FileFingerprint::new(
            "src/generated.cache",
            "hash-b",
            FileDisposition::Ignored,
        )],
    )
    .with_artifacts([("target/report.json", "artifact-hash")]);
    let default_check = command_check();
    let strict_check = command_check().include_ignored(true);

    assert_eq!(
        command_evidence(&default_check, &verified).status_against(&changed),
        EvidenceStatus::Passed
    );
    assert_eq!(
        command_evidence(&strict_check, &verified).status_against(&changed),
        EvidenceStatus::Stale
    );
}

#[test]
fn provider_claim_never_satisfies_a_required_check() {
    let definition = mission_definition("mission-1");
    let check = command_check().covers(definition.acceptance_criterion_ids());
    let lifecycle = MissionLifecycle::draft(definition, 0)
        .with_run_target(
            run_target("/repo/.worktrees/mission-1"),
            std::slice::from_ref(&check),
            "test",
            1,
        )
        .unwrap();
    let records = BTreeMap::from([(
        check.id().to_owned(),
        EvidenceRecord::ProviderClaim(ProviderClaim::new(
            check.id(),
            "Tests pass",
            "structured_hook",
            100,
        )),
    )]);

    let state = workspace("a");
    let report = evaluate_test_proof(
        &lifecycle,
        &[check],
        &records,
        &state,
        &std::collections::BTreeSet::new(),
        1,
        100,
    )
    .unwrap();
    assert_eq!(report.readiness(), MissionReadiness::ReviewRequired);
}

#[test]
fn zero_required_checks_can_never_mint_a_verified_proof() {
    let definition = mission_definition("mission-1");
    let check = command_check()
        .optional()
        .covers(definition.acceptance_criterion_ids());
    let error = MissionLifecycle::draft(definition, 0)
        .with_run_target(
            run_target("/repo/.worktrees/mission-1"),
            &[check],
            "test",
            1,
        )
        .unwrap_err();

    assert_eq!(error.to_string(), "mission closure plan is invalid");
}

#[test]
fn evidence_from_a_weaker_declaration_cannot_satisfy_a_stronger_check_with_the_same_id() {
    let definition = mission_definition("mission-1");
    let criteria = definition.acceptance_criterion_ids();
    let weak = CheckDeclaration::command(
        "tests",
        CommandSpec::new("cargo", ["test"], "."),
        vec![PathRule::prefix("src/")],
        vec![],
    );
    let strong = CheckDeclaration::command(
        "tests",
        CommandSpec::new("cargo", ["test", "--all-features"], "."),
        vec![PathRule::prefix("src/")],
        vec![ArtifactRequirement::new("target/report.json")],
    )
    .covers(criteria);
    let lifecycle = MissionLifecycle::draft(definition, 0)
        .with_run_target(
            run_target("/repo/.worktrees/mission-1"),
            std::slice::from_ref(&strong),
            "test",
            1,
        )
        .unwrap();
    let state = workspace("a");
    let evidence = CommandEvidence::new(
        &weak,
        &proof_identity("mission-1"),
        &state,
        &state,
        0,
        100,
        150,
        vec![],
    )
    .unwrap();
    let records = BTreeMap::from([(
        "tests".to_owned(),
        EvidenceRecord::Command(Box::new(evidence)),
    )]);

    let report = evaluate_test_proof(
        &lifecycle,
        &[strong],
        &records,
        &state,
        &std::collections::BTreeSet::new(),
        1,
        200,
    )
    .unwrap();

    assert_eq!(
        report.check_status("tests"),
        Some(CheckProofStatus::DeclarationMismatch)
    );
    assert_eq!(report.readiness(), MissionReadiness::ReviewRequired);
}

#[test]
fn mission_requires_every_fresh_check_and_zero_blockers() {
    let definition = mission_definition("mission-1");
    let command = command_check().covers(definition.acceptance_criterion_ids());
    let manual = CheckDeclaration::manual("visual-review").reviewed_by(["reviewer"]);
    let lifecycle = MissionLifecycle::draft(definition, 0)
        .with_run_target(
            run_target("/repo/.worktrees/mission-1"),
            &[command.clone(), manual.clone()],
            "test",
            1,
        )
        .unwrap();
    let state = workspace("a");
    let records = BTreeMap::from([
        (
            command.id().to_owned(),
            EvidenceRecord::Command(Box::new(command_evidence(&command, &state))),
        ),
        (
            manual.id().to_owned(),
            EvidenceRecord::Manual(
                ManualEvidence::new(
                    &manual,
                    &proof_identity("mission-1"),
                    &state,
                    "reviewer",
                    200,
                    "Readable at 80x24",
                    false,
                )
                .unwrap(),
            ),
        ),
    ]);

    let verified = evaluate_test_proof(
        &lifecycle,
        &[command.clone(), manual.clone()],
        &records,
        &state,
        &std::collections::BTreeSet::new(),
        1,
        200,
    )
    .unwrap();
    let blockers = std::collections::BTreeSet::from(["attention-1".to_owned()]);
    let blocked = evaluate_test_proof(
        &lifecycle,
        &[command, manual],
        &records,
        &state,
        &blockers,
        2,
        201,
    )
    .unwrap();
    assert_eq!(verified.readiness(), MissionReadiness::Verified);
    assert_eq!(blocked.readiness(), MissionReadiness::ReviewRequired);
}

#[test]
fn attention_keeps_navigation_audit_and_read_state() {
    let empty = AttentionInbox::new();
    assert!(empty.is_empty());

    let inbox = empty
        .ingest(permission(
            "event-1",
            "request-1",
            "worktree-a",
            AttentionRisk::High,
        ))
        .unwrap();
    let item = inbox.items().next().unwrap();
    assert_eq!(item.mission_id(), "mission-1");
    assert_eq!(item.session_id(), "session-1");
    assert_eq!(item.pane_target().workspace(), "workspace-1");
    assert_eq!(item.pane_target().pane(), "terminal-7");
    assert_eq!(item.requested_action(), "Run database migration");
    assert_eq!(item.source(), ObservationSource::StructuredHook);
    assert_eq!(item.kind(), AttentionKind::PermissionRequest);
    assert_eq!(item.scope(), "worktree-a");
    assert_eq!(item.risk(), AttentionRisk::High);
    assert!(item.is_unread());
    assert_eq!(item.created_at_millis(), 10);
    assert_eq!(item.expires_at_millis(), None);

    let item_id = item.id().to_owned();
    let read = inbox.mark_read(&item_id).unwrap();
    assert_eq!(read.unread_count(), 0);
    assert!(read
        .dismiss(&item_id, "reviewer", "handled in native pane", 20)
        .is_err());
    assert_eq!(
        read.item(&item_id).unwrap().status(),
        &AttentionStatus::Open
    );
}

#[test]
fn command_and_manual_evidence_preserve_the_full_audit_record() {
    let state = workspace("a");
    let check = CheckDeclaration::command(
        "exact-check",
        CommandSpec::new("cargo", ["test", "--locked"], "."),
        vec![PathRule::exact("src/lib.rs")],
        vec![ArtifactRequirement::new("target/report.json")],
    )
    .optional();
    let evidence = command_evidence(&check, &state);

    assert_eq!(evidence.check_id(), "exact-check");
    assert_eq!(evidence.command().program(), "cargo");
    assert_eq!(evidence.command().args(), ["test", "--locked"]);
    assert_eq!(evidence.command().cwd(), ".");
    assert_eq!(evidence.base_tree_hash(), "tree-a");
    assert_eq!(evidence.result_tree_hash(), "tree-a");
    assert_eq!(evidence.diff_hash(), "diff-a");
    assert_eq!(evidence.exit_code(), 0);
    assert_eq!(evidence.duration_millis(), 50);
    assert_eq!(evidence.artifacts()[0].content_hash(), "artifact-hash");

    let manual_check = CheckDeclaration::manual("visual-review")
        .reviewed_by(["reviewer"])
        .allow_manual_override();
    let manual = ManualEvidence::new(
        &manual_check,
        &proof_identity("mission-1"),
        &state,
        "reviewer",
        250,
        "Readable at 80x24",
        true,
    )
    .unwrap();
    assert_eq!(manual.check_id(), "visual-review");
    assert_eq!(manual.author(), "reviewer");
    assert_eq!(manual.recorded_at_millis(), 250);
    assert_eq!(manual.reason(), "Readable at 80x24");
    assert!(manual.is_override());
}

#[test]
fn a_new_turn_can_follow_a_completed_turn() {
    let completed = SessionSnapshot::starting(0).apply(
        structured("complete-1", SessionStatus::Ready, 5, 5, "turn-1", 50).with_turn_completed(),
    );
    let next = completed.apply(structured(
        "start-2",
        SessionStatus::Working,
        6,
        6,
        "turn-2",
        60,
    ));

    assert_eq!(next.current_turn_id(), Some("turn-2"));
    assert!(!next.current_turn_completed());
}

proptest! {
    #[test]
    fn every_relevant_content_mutation_stales_evidence(
        before in "[a-z]{1,32}",
        after in "[A-Z]{1,32}",
    ) {
        let check = command_check();
        let verified = workspace(&before);
        let changed = workspace(&after);
        let evidence = command_evidence(&check, &verified);

        prop_assert_eq!(evidence.status_against(&changed), EvidenceStatus::Stale);
    }

    #[test]
    fn irrelevant_content_mutation_keeps_evidence_fresh(
        before in "[a-z0-9]{1,32}",
        after in "[A-Z0-9]{1,32}",
    ) {
        let check = command_check();
        let verified = WorkspaceSnapshot::new(
            "tree-a",
            "diff-a",
            vec![FileFingerprint::new(
                "docs/guide.md",
                before,
                FileDisposition::Tracked,
            )],
        )
        .with_artifacts([("target/report.json", "artifact-hash")]);
        let changed = WorkspaceSnapshot::new(
            "tree-b",
            "diff-b",
            vec![FileFingerprint::new(
                "docs/guide.md",
                after,
                FileDisposition::Tracked,
            )],
        )
        .with_artifacts([("target/report.json", "artifact-hash")]);
        let evidence = command_evidence(&check, &verified);

        prop_assert_eq!(evidence.status_against(&changed), EvidenceStatus::Passed);
    }
}

#[test]
fn mission_definition_rejects_ambiguous_or_unscoped_work() {
    let missing_objective = MissionDefinition::new(
        "mission-1",
        "Fix login",
        "/repo",
        "  ",
        ["The redirect test passes"],
    )
    .unwrap_err();
    let missing_acceptance = MissionDefinition::new(
        "mission-1",
        "Fix login",
        "/repo",
        "Users land on the requested page",
        std::iter::empty::<&str>(),
    )
    .unwrap_err();
    let relative_repository = MissionDefinition::new(
        "mission-1",
        "Fix login",
        "repo",
        "Users land on the requested page",
        ["The redirect test passes"],
    )
    .unwrap_err();

    assert_eq!(
        missing_objective.to_string(),
        "mission objective cannot be empty"
    );
    assert_eq!(
        missing_acceptance.to_string(),
        "mission needs at least one acceptance criterion"
    );
    assert_eq!(
        relative_repository.to_string(),
        "repository path must be absolute"
    );
}

#[test]
fn mission_lifecycle_cannot_skip_safety_gates() {
    let lifecycle = MissionLifecycle::draft(mission_definition("mission-1"), 10);

    let error = lifecycle
        .clone()
        .transition(MissionStatus::Active, "user", "start", 20)
        .unwrap_err();
    let preparing = lifecycle
        .with_run_target(
            run_target("/repo/.worktrees/mission-1"),
            &[closure_check("mission-1")],
            "user",
            20,
        )
        .unwrap();

    assert_eq!(
        error.to_string(),
        "invalid mission transition: draft -> active"
    );
    assert_eq!(preparing.status(), MissionStatus::Preparing);
    assert_eq!(preparing.history().len(), 2);
}

#[test]
fn verified_proof_is_required_before_a_mission_is_ready_to_close() {
    let active = MissionLifecycle::draft(mission_definition("mission-1"), 10)
        .with_run_target(
            run_target("/repo/.worktrees/mission-1"),
            &[closure_check("mission-1")],
            "user",
            20,
        )
        .unwrap()
        .transition(MissionStatus::Active, "runtime", "provider ready", 30)
        .unwrap()
        .transition(
            MissionStatus::ReviewRequired,
            "runtime",
            "turn completed",
            40,
        )
        .unwrap();

    let generic_rejected =
        active
            .clone()
            .transition(MissionStatus::ReadyToClose, "user", "looks good", 50);
    let archive_premint = archive_proof_at(&active, 60, 50);
    let wrong_scope = active.clone().mark_ready_to_close(
        ready_proof_for("mission-2", "/repo/.worktrees/mission-1", 50),
        "user",
        "checks verified",
        50,
    );
    let ready = active
        .mark_ready_to_close(
            ready_proof_for("mission-1", "/repo/.worktrees/mission-1", 50),
            "user",
            "checks verified",
            50,
        )
        .unwrap();

    assert_eq!(
        generic_rejected.unwrap_err().to_string(),
        "mission transition to ready_to_close requires a sealed proof command"
    );
    assert_eq!(
        archive_premint.unwrap_err().to_string(),
        "invalid mission transition: review_required -> archived"
    );
    assert_eq!(
        wrong_scope.unwrap_err().to_string(),
        "verified proof belongs to another mission run or worktree"
    );
    assert_eq!(ready.status(), MissionStatus::ReadyToClose);

    let not_fresh = archive_proof_at(&ready, 50, 50);
    assert_eq!(
        not_fresh.unwrap_err().to_string(),
        "archiving requires a newly evaluated proof"
    );
    let later_clock_without_a_new_authority_revision = archive_proof_at(&ready, 50, 60);
    assert_eq!(
        later_clock_without_a_new_authority_revision
            .unwrap_err()
            .to_string(),
        "archiving requires a newly evaluated proof"
    );
    let archive_proof = archive_proof_at(&ready, 60, 60).unwrap();
    let archived = ready
        .archive_with_fresh_proof(archive_proof, "user", "archive", 60)
        .unwrap();
    assert_eq!(archived.status(), MissionStatus::Archived);
}

#[test]
fn invented_criteria_cannot_close_a_real_mission() {
    let lifecycle = MissionLifecycle::draft(mission_definition("mission-1"), 10)
        .with_run_target(
            run_target("/repo/.worktrees/mission-1"),
            &[closure_check("mission-1")],
            "user",
            20,
        )
        .unwrap()
        .transition(MissionStatus::Active, "system", "agent started", 30)
        .unwrap()
        .transition(MissionStatus::ReviewRequired, "system", "agent stopped", 40)
        .unwrap();
    let fake_criterion = vec!["invented-criterion".to_owned()];
    let fake_check = command_check().covers(fake_criterion.clone());
    let state = workspace("a");
    let fake_evidence = command_evidence(&fake_check, &state);
    let records = BTreeMap::from([(
        fake_check.id().to_owned(),
        EvidenceRecord::Command(Box::new(fake_evidence)),
    )]);
    let report = evaluate_test_proof(
        &lifecycle,
        &[fake_check],
        &records,
        &state,
        &std::collections::BTreeSet::new(),
        1,
        50,
    )
    .unwrap();

    assert_eq!(report.readiness(), MissionReadiness::ReviewRequired);
    assert!(!report.uncovered_criteria().is_empty());
    assert!(report.duplicate_check_ids().is_empty());
    assert_eq!(
        report.into_verified().unwrap_err().to_string(),
        "proof report is not verified"
    );
}

#[test]
fn a_worktree_has_exactly_one_active_writer() {
    let directory = tempfile::tempdir().unwrap();
    let registry = WorktreeClaimRegistry::new(directory.path().join("locks")).unwrap();
    let clone = registry.clone();
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let first_owner = LeaseOwner::new("mission-1", "run-1").unwrap();
    let second_owner = LeaseOwner::new("mission-2", "run-1").unwrap();
    let first_request = ClaimRequestId::new("claim-1").unwrap();
    let second_request = ClaimRequestId::new("claim-2").unwrap();

    let first = registry
        .claim(
            first_owner.clone(),
            repository,
            repository,
            first_request.clone(),
        )
        .unwrap();
    let idempotent = clone
        .claim(first_owner, repository, repository, first_request)
        .unwrap();
    let conflict = clone
        .claim(
            second_owner.clone(),
            repository,
            repository,
            second_request.clone(),
        )
        .unwrap_err();

    assert_eq!(first, idempotent);
    assert_eq!(
        first.checkout_root(),
        std::fs::canonicalize(repository).unwrap()
    );
    assert_eq!(
        conflict.to_string(),
        "worktree is already owned by mission mission-1 run run-1"
    );
    assert_eq!(registry.release(&first).unwrap(), ReleaseOutcome::Released);
    let second = clone
        .claim(second_owner.clone(), repository, repository, second_request)
        .unwrap();
    assert_eq!(clone.owner(repository), Some(second_owner));
    assert_eq!(
        registry.release(&first).unwrap_err().to_string(),
        "worktree lease is stale"
    );
    assert_eq!(registry.release(&second).unwrap(), ReleaseOutcome::Released);
    assert_eq!(
        registry.release(&second).unwrap(),
        ReleaseOutcome::AlreadyReleased
    );
}

#[test]
fn worktree_writer_lock_is_exclusive_across_independent_registries() {
    let directory = tempfile::tempdir().unwrap();
    let lock_directory = directory.path().join("locks");
    let first_registry = WorktreeClaimRegistry::new(&lock_directory).unwrap();
    let second_registry = WorktreeClaimRegistry::new(&lock_directory).unwrap();
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lease = first_registry
        .claim(
            LeaseOwner::new("mission-1", "run-1").unwrap(),
            repository,
            repository,
            ClaimRequestId::new("claim-1").unwrap(),
        )
        .unwrap();

    let error = second_registry
        .claim(
            LeaseOwner::new("mission-2", "run-1").unwrap(),
            repository,
            repository,
            ClaimRequestId::new("claim-2").unwrap(),
        )
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "worktree is already owned by another process"
    );
    assert_eq!(
        first_registry.release(&lease).unwrap(),
        ReleaseOutcome::Released
    );
}

#[test]
fn lifecycle_history_serializes_for_audit_without_becoming_recovery_authority() {
    let lifecycle = MissionLifecycle::draft(mission_definition("mission-1"), 10)
        .with_run_target(
            RunTarget::new(
                "run-1",
                ProviderKind::OpenCode,
                ProviderMode::Passthrough,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "/repo/.worktrees/mission-1",
            )
            .unwrap(),
            &[closure_check("mission-1")],
            "user",
            20,
        )
        .unwrap();

    let audit = serde_json::to_value(&lifecycle).unwrap();

    assert_eq!(
        lifecycle.run_target().unwrap().mode(),
        ProviderMode::Passthrough
    );
    assert_eq!(audit["status"], "preparing");
    assert_eq!(audit["history"].as_array().unwrap().len(), 2);
    assert_eq!(audit["run_target"]["mode"], "passthrough");
    assert!(audit["closure_plan"].is_object());
}

fn persisted_created(mission_id: &str) -> PersistableMissionEvent {
    PersistableMissionEvent::mission_created(
        mission_id,
        "Fix login redirect",
        "/repo",
        "Users land on the requested page after login",
        vec!["The redirect test passes".to_owned()],
        10,
    )
    .unwrap()
}

fn persisted_response_route() -> PersistedResponseRoute {
    PersistedResponseRoute::new(
        ProviderKind::Codex,
        "run-1",
        "session-1",
        PaneTarget::new("workspace-1", "pane-1"),
        "provider-request-1",
    )
}

fn seed_persisted_attention(store: &mut MissionStore) {
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    store
        .commit(
            "seed-closure",
            PersistableMissionEvent::closure_configured(
                "mission-1",
                vec![CheckDeclaration::command(
                    "test",
                    CommandSpec::new("cargo", ["test"], "."),
                    vec![PathRule::All],
                    Vec::new(),
                )
                .covers(MissionDefinition::criterion_ids(&[
                    "The redirect test passes".to_owned(),
                ]))],
                11,
            )
            .unwrap(),
        )
        .unwrap();
    store
        .commit(
            "seed-run",
            PersistableMissionEvent::run_started(
                "mission-1",
                "run-1",
                ProviderKind::Codex,
                ProviderMode::Passthrough,
                "/repo",
                "a".repeat(40),
                12,
            )
            .unwrap(),
        )
        .unwrap();
    store
        .commit(
            "seed-session",
            PersistableMissionEvent::provider_session_bound("mission-1", "run-1", "session-1", 13)
                .unwrap(),
        )
        .unwrap();
    store
        .commit(
            "event-2",
            PersistableMissionEvent::AttentionChanged {
                mission_id: "mission-1".to_owned(),
                attention_id: "attention-1".to_owned(),
                state: PersistedAttentionState::Open,
                risk: AttentionRisk::High,
                at_millis: 20,
            },
        )
        .unwrap();
}

#[test]
fn mission_store_roundtrips_checkpoint_and_replays_newer_events() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    store.checkpoint().unwrap();
    store
        .commit(
            "event-2",
            PersistableMissionEvent::StatusChanged {
                mission_id: "mission-1".to_owned(),
                status: MissionStatus::Preparing,
                at_millis: 20,
            },
        )
        .unwrap();
    drop(store);

    let restored = MissionStore::open(directory.path()).unwrap();

    assert_eq!(restored.last_sequence(), 2);
    assert_eq!(
        restored.projection().mission_status("mission-1"),
        Some(MissionStatus::Preparing)
    );
    assert_eq!(
        restored
            .events_after(1)
            .map(|event| event.sequence())
            .collect::<Vec<_>>(),
        vec![2]
    );
}

#[test]
fn provider_response_request_and_ack_survive_crash_replay() {
    let directory = tempfile::tempdir().unwrap();
    let key = ResponseAttemptKey::new("attention-1", 1, 1).unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    seed_persisted_attention(&mut store);
    store
        .commit(
            "event-3",
            PersistableMissionEvent::ResponseRequested {
                mission_id: "mission-1".to_owned(),
                key: key.clone(),
                route: persisted_response_route(),
                decision: AttentionDecision::ApproveOnce,
                actor_id: "local-user".to_owned(),
                at_millis: 30,
            },
        )
        .unwrap();
    drop(store);

    let mut recovered = MissionStore::open(directory.path()).unwrap();
    assert_eq!(
        recovered.projection().response_state("mission-1", &key),
        Some(&PersistedResponseState::Requested)
    );
    assert_eq!(
        recovered
            .projection()
            .attention_state("mission-1", "attention-1"),
        Some(PersistedAttentionState::PendingResponse)
    );
    recovered
        .commit(
            "event-4",
            PersistableMissionEvent::ResponseAcknowledged {
                mission_id: "mission-1".to_owned(),
                key: key.clone(),
                acknowledgement_hash: Some("a".repeat(64)),
                at_millis: 40,
            },
        )
        .unwrap();
    assert!(matches!(
        recovered.projection().response_state("mission-1", &key),
        Some(PersistedResponseState::Acknowledged { .. })
    ));
    assert_eq!(
        recovered
            .projection()
            .attention_state("mission-1", "attention-1"),
        Some(PersistedAttentionState::Resolved)
    );
}

#[test]
fn unknown_provider_delivery_requires_reconciliation_and_never_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let key = ResponseAttemptKey::new("attention-1", 1, 1).unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    seed_persisted_attention(&mut store);
    store
        .commit(
            "event-3",
            PersistableMissionEvent::ResponseRequested {
                mission_id: "mission-1".to_owned(),
                key: key.clone(),
                route: persisted_response_route(),
                decision: AttentionDecision::Deny,
                actor_id: "local-user".to_owned(),
                at_millis: 30,
            },
        )
        .unwrap();
    store
        .commit(
            "event-4",
            PersistableMissionEvent::ResponseFailed {
                mission_id: "mission-1".to_owned(),
                key: key.clone(),
                disposition: ResponseFailureDisposition::DeliveryUnknown,
                code: ResponseFailureCode::Timeout,
                at_millis: 40,
            },
        )
        .unwrap();

    assert!(matches!(
        store.projection().response_state("mission-1", &key),
        Some(PersistedResponseState::ReconciliationRequired {
            code: ResponseFailureCode::Timeout
        })
    ));
    assert_eq!(
        store
            .projection()
            .attention_state("mission-1", "attention-1"),
        Some(PersistedAttentionState::ReconciliationRequired)
    );
    let retry = store.commit(
        "event-5",
        PersistableMissionEvent::ResponseRequested {
            mission_id: "mission-1".to_owned(),
            key: ResponseAttemptKey::new("attention-1", 1, 2).unwrap(),
            route: persisted_response_route(),
            decision: AttentionDecision::Deny,
            actor_id: "local-user".to_owned(),
            at_millis: 50,
        },
    );
    assert!(retry.is_err());
    assert_eq!(store.last_sequence(), 7);
}

#[test]
fn definitely_unapplied_response_reopens_for_a_new_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let first_key = ResponseAttemptKey::new("attention-1", 1, 1).unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    seed_persisted_attention(&mut store);
    store
        .commit(
            "event-3",
            PersistableMissionEvent::ResponseRequested {
                mission_id: "mission-1".to_owned(),
                key: first_key.clone(),
                route: persisted_response_route(),
                decision: AttentionDecision::Deny,
                actor_id: "local-user".to_owned(),
                at_millis: 30,
            },
        )
        .unwrap();
    store
        .commit(
            "event-4",
            PersistableMissionEvent::ResponseFailed {
                mission_id: "mission-1".to_owned(),
                key: first_key,
                disposition: ResponseFailureDisposition::DefinitelyNotApplied,
                code: ResponseFailureCode::DisconnectedBeforeWrite,
                at_millis: 40,
            },
        )
        .unwrap();
    let retry_key = ResponseAttemptKey::new("attention-1", 1, 2).unwrap();
    store
        .commit(
            "event-5",
            PersistableMissionEvent::ResponseRequested {
                mission_id: "mission-1".to_owned(),
                key: retry_key.clone(),
                route: persisted_response_route(),
                decision: AttentionDecision::Deny,
                actor_id: "local-user".to_owned(),
                at_millis: 50,
            },
        )
        .unwrap();

    assert_eq!(
        store.projection().response_state("mission-1", &retry_key),
        Some(&PersistedResponseState::Requested)
    );
}

#[test]
fn provider_ack_without_a_durable_request_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    seed_persisted_attention(&mut store);

    let result = store.commit(
        "event-3",
        PersistableMissionEvent::ResponseAcknowledged {
            mission_id: "mission-1".to_owned(),
            key: ResponseAttemptKey::new("attention-1", 1, 1).unwrap(),
            acknowledgement_hash: None,
            at_millis: 30,
        },
    );

    assert!(result.is_err());
    assert_eq!(store.last_sequence(), 5);
}

#[test]
fn generic_attention_events_cannot_forge_response_states() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    seed_persisted_attention(&mut store);

    for (index, state) in [
        PersistedAttentionState::PendingResponse,
        PersistedAttentionState::ReconciliationRequired,
        PersistedAttentionState::Resolved,
        PersistedAttentionState::Dismissed,
    ]
    .into_iter()
    .enumerate()
    {
        let error = store
            .commit(
                &format!("forged-attention-{index}"),
                PersistableMissionEvent::AttentionChanged {
                    mission_id: "mission-1".to_owned(),
                    attention_id: "attention-1".to_owned(),
                    state,
                    risk: AttentionRisk::High,
                    at_millis: 30,
                },
            )
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "attention state must be changed by its dedicated durable event"
        );
        assert_eq!(store.last_sequence(), 5);
    }
}

#[test]
fn mission_store_uses_logical_time_when_the_observed_clock_moves_backwards() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    seed_persisted_attention(&mut store);

    store
        .commit(
            "event-old",
            PersistableMissionEvent::EvidenceChanged {
                mission_id: "mission-1".to_owned(),
                check_id: "check-1".to_owned(),
                status: EvidenceStatus::Passed,
                workspace_hash: "b".repeat(64),
                at_millis: 19,
            },
        )
        .unwrap();

    assert_eq!(store.last_sequence(), 6);
    assert_eq!(
        store
            .projection()
            .mission_view("mission-1")
            .unwrap()
            .updated_at_millis,
        20
    );
}

#[test]
fn mission_store_allows_exactly_one_writer_and_releases_the_lock_on_drop() {
    let directory = tempfile::tempdir().unwrap();
    let first = MissionStore::open(directory.path()).unwrap();

    assert_eq!(
        MissionStore::open(directory.path())
            .unwrap_err()
            .to_string(),
        "mission store already has an active writer"
    );
    drop(first);
    assert!(MissionStore::open(directory.path()).is_ok());
}

#[test]
fn prepared_store_keeps_the_writer_lock_until_relinquish() {
    let directory = tempfile::tempdir().unwrap();
    let store = MissionStore::open(directory.path()).unwrap();
    let prepared = store.prepare_handoff().unwrap();
    let fence = prepared.fence();

    assert_eq!(fence.sequence(), 0);
    assert!(MissionStore::open(directory.path()).is_err());

    let reader = MissionStoreReader::open_existing(directory.path()).unwrap();
    let released = prepared.relinquish();
    let successor = reader.acquire_writer(fence).unwrap();
    assert_eq!(successor.last_sequence(), 0);
    drop(successor);
    drop(released);
}

#[test]
fn prepared_handoff_can_abort_without_losing_writer_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let store = MissionStore::open(directory.path()).unwrap();
    let prepared = store.prepare_handoff().unwrap();
    let mut restored = prepared.abort();

    let outcome = restored
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    assert_eq!(outcome.sequence(), 1);
    assert!(MissionStore::open(directory.path()).is_err());
}

#[test]
fn handoff_successor_replays_again_after_acquiring_the_lock() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    let reader = MissionStoreReader::open_existing(directory.path()).unwrap();
    assert_eq!(reader.observed_fence().sequence(), 1);
    store
        .commit(
            "event-2",
            PersistableMissionEvent::StatusChanged {
                mission_id: "mission-1".to_owned(),
                status: MissionStatus::Preparing,
                at_millis: 20,
            },
        )
        .unwrap();
    let prepared = store.prepare_handoff().unwrap();
    let fence = prepared.fence();
    let _released = prepared.relinquish();

    let successor = reader.acquire_writer(fence).unwrap();
    assert_eq!(successor.last_sequence(), 2);
    assert_eq!(
        successor.projection().mission_status("mission-1"),
        Some(MissionStatus::Preparing)
    );
}

#[test]
fn stale_handoff_fence_is_rejected_after_another_writer_commits() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    let prepared = store.prepare_handoff().unwrap();
    let stale_fence = prepared.fence();
    let released = prepared.relinquish();
    let mut writer = released.reacquire().unwrap();
    writer
        .commit(
            "event-2",
            PersistableMissionEvent::StatusChanged {
                mission_id: "mission-1".to_owned(),
                status: MissionStatus::Preparing,
                at_millis: 20,
            },
        )
        .unwrap();
    drop(writer);

    let reader = MissionStoreReader::open_existing(directory.path()).unwrap();
    assert!(reader.acquire_writer(stale_fence).is_err());
}

#[test]
fn handoff_rollback_reacquires_and_continues_at_the_next_sequence() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    let prepared = store.prepare_handoff().unwrap();
    let released = prepared.relinquish();
    let mut restored = released.reacquire().unwrap();

    let outcome = restored
        .commit(
            "event-2",
            PersistableMissionEvent::StatusChanged {
                mission_id: "mission-1".to_owned(),
                status: MissionStatus::Preparing,
                at_millis: 20,
            },
        )
        .unwrap();
    assert_eq!(outcome.sequence(), 2);
}

#[test]
fn read_only_handoff_inspection_never_repairs_a_partial_tail() {
    use std::io::Write as _;

    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    drop(store);
    let journal = directory.path().join("missions/missions.journal.bin");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap();
    file.write_all(b"MSN").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let length_with_partial_tail = std::fs::metadata(&journal).unwrap().len();

    assert!(MissionStoreReader::open_existing(directory.path()).is_err());
    assert_eq!(
        std::fs::metadata(&journal).unwrap().len(),
        length_with_partial_tail
    );
    assert_eq!(
        MissionStore::open(directory.path())
            .unwrap()
            .last_sequence(),
        1
    );
    assert!(std::fs::metadata(journal).unwrap().len() < length_with_partial_tail);
}

#[test]
fn duplicate_event_id_with_a_different_payload_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();

    let error = store
        .commit("event-1", persisted_created("mission-2"))
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "mission event id event-1 was reused with a different payload"
    );
    assert_eq!(store.last_sequence(), 1);
}

#[test]
fn mission_store_refuses_invalid_and_unsealed_status_transitions() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();

    let skipped = store
        .commit(
            "event-2",
            PersistableMissionEvent::StatusChanged {
                mission_id: "mission-1".to_owned(),
                status: MissionStatus::Active,
                at_millis: 20,
            },
        )
        .unwrap_err();
    let sealed = store
        .commit(
            "event-3",
            PersistableMissionEvent::StatusChanged {
                mission_id: "mission-1".to_owned(),
                status: MissionStatus::Archived,
                at_millis: 30,
            },
        )
        .unwrap_err();

    assert_eq!(
        skipped.to_string(),
        "invalid persisted mission transition: draft -> active"
    );
    assert_eq!(
        sealed.to_string(),
        "ready_to_close and archived require sealed mission events"
    );
    assert_eq!(store.last_sequence(), 1);
}

#[test]
fn sealed_ready_and_archive_states_survive_journal_replay() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    for (event_id, status, at_millis) in [
        ("event-2", MissionStatus::Preparing, 20),
        ("event-3", MissionStatus::Active, 30),
        ("event-4", MissionStatus::ReviewRequired, 40),
    ] {
        store
            .commit(
                event_id,
                PersistableMissionEvent::StatusChanged {
                    mission_id: "mission-1".to_owned(),
                    status,
                    at_millis,
                },
            )
            .unwrap();
    }

    let ready_head = store.handoff_fence().authority_digest();
    let ready_event = PersistableMissionEvent::mission_ready(
        "mission-1",
        ready_proof_at_head(
            "mission-1",
            "/repo/.worktrees/mission-1",
            4,
            50,
            Some(&ready_head),
        ),
        "reviewer",
        "c".repeat(64),
        50,
    )
    .unwrap();
    store.commit("event-5", ready_event).unwrap();

    let ready_lifecycle = MissionLifecycle::draft(mission_definition("mission-1"), 10)
        .with_run_target(
            run_target("/repo/.worktrees/mission-1"),
            &[closure_check("mission-1")],
            "user",
            20,
        )
        .unwrap()
        .transition(MissionStatus::Active, "runtime", "provider ready", 30)
        .unwrap()
        .transition(
            MissionStatus::ReviewRequired,
            "runtime",
            "turn completed",
            40,
        )
        .unwrap()
        .mark_ready_to_close(
            ready_proof_at_head(
                "mission-1",
                "/repo/.worktrees/mission-1",
                4,
                50,
                Some(&ready_head),
            ),
            "reviewer",
            "checks verified",
            50,
        )
        .unwrap();
    let archive_head = store.handoff_fence().authority_digest();
    let archive_event = PersistableMissionEvent::mission_archived(
        "mission-1",
        archive_proof_at_head(&ready_lifecycle, 5, 60, Some(&archive_head)).unwrap(),
        "reviewer",
        "d".repeat(64),
        60,
    )
    .unwrap();
    store.commit("event-6", archive_event).unwrap();
    drop(store);

    let restored = MissionStore::open(directory.path()).unwrap();
    assert_eq!(
        restored.projection().mission_status("mission-1"),
        Some(MissionStatus::Archived)
    );
    assert_eq!(restored.last_sequence(), 6);
}

#[test]
fn sealed_proof_cannot_commit_after_the_authority_head_changes() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    for (event_id, status, at_millis) in [
        ("event-2", MissionStatus::Preparing, 20),
        ("event-3", MissionStatus::Active, 30),
        ("event-4", MissionStatus::ReviewRequired, 40),
    ] {
        store
            .commit(
                event_id,
                PersistableMissionEvent::StatusChanged {
                    mission_id: "mission-1".to_owned(),
                    status,
                    at_millis,
                },
            )
            .unwrap();
    }

    let authority_head = store.handoff_fence().authority_digest();
    let stale_ready = PersistableMissionEvent::mission_ready(
        "mission-1",
        ready_proof_at_head(
            "mission-1",
            "/repo/.worktrees/mission-1",
            4,
            50,
            Some(&authority_head),
        ),
        "reviewer",
        "c".repeat(64),
        50,
    )
    .unwrap();
    store
        .commit(
            "event-5",
            PersistableMissionEvent::EvidenceChanged {
                mission_id: "mission-1".to_owned(),
                check_id: "tests".to_owned(),
                status: EvidenceStatus::Stale,
                workspace_hash: "e".repeat(64),
                at_millis: 45,
            },
        )
        .unwrap();

    let error = store.commit("event-6", stale_ready).unwrap_err();
    assert_eq!(error.to_string(), "sealed proof authority is stale");
    assert_eq!(store.last_sequence(), 5);
    assert_eq!(
        store.projection().mission_status("mission-1"),
        Some(MissionStatus::ReviewRequired)
    );
}

#[test]
fn sealed_store_event_rejects_a_proof_for_another_mission() {
    let error = PersistableMissionEvent::mission_ready(
        "mission-1",
        ready_proof_at("mission-2", "/repo/.worktrees/mission-2", 4, 50),
        "reviewer",
        "c".repeat(64),
        50,
    )
    .unwrap_err();

    assert_eq!(error.to_string(), "sealed proof belongs to another mission");
}

#[test]
fn mission_store_deduplicates_event_ids_without_allocating_a_sequence() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();

    let first = store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    let duplicate = store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();

    assert_eq!(first.sequence(), 1);
    assert!(!first.was_duplicate());
    assert_eq!(duplicate.sequence(), 1);
    assert!(duplicate.was_duplicate());
    assert_eq!(store.last_sequence(), 1);
}

#[test]
fn oversized_mission_is_rejected_without_poisoning_the_writer() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    let error = PersistableMissionEvent::mission_created(
        "oversized",
        "Large but otherwise valid mission",
        "/repo",
        "o".repeat(16 * 1024),
        (0..32)
            .map(|index| format!("criterion-{index}:{}", "x".repeat(2_000)))
            .collect(),
        10,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        super::store::MissionStoreError::InvalidText {
            label: "mission objective"
        }
    ));
    assert_eq!(store.last_sequence(), 0);

    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    assert_eq!(store.last_sequence(), 1);
}

#[test]
fn mission_store_recovers_only_a_truncated_final_record() {
    use std::io::Write as _;

    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    drop(store);

    let journal = directory.path().join("missions/missions.journal.bin");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap();
    file.write_all(b"MSN").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let mut recovered = MissionStore::open(directory.path()).unwrap();
    recovered
        .commit(
            "event-2",
            PersistableMissionEvent::StatusChanged {
                mission_id: "mission-1".to_owned(),
                status: MissionStatus::Preparing,
                at_millis: 20,
            },
        )
        .unwrap();
    drop(recovered);

    assert_eq!(
        MissionStore::open(directory.path())
            .unwrap()
            .last_sequence(),
        2
    );
}

#[test]
fn mission_store_refuses_corruption_before_the_end_of_the_journal() {
    use std::io::Write as _;

    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    drop(store);

    let journal = directory.path().join("missions/missions.journal.bin");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap();
    file.write_all(b"not-a-journal-frame").unwrap();
    file.sync_all().unwrap();
    drop(file);

    assert!(MissionStore::open(directory.path()).is_err());
}

#[test]
fn mission_store_detects_a_single_payload_byte_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    drop(store);

    let journal = directory.path().join("missions/missions.journal.bin");
    let mut bytes = std::fs::read(&journal).unwrap();
    let payload_length = u32::from_be_bytes(bytes[10..14].try_into().unwrap()) as usize;
    assert!(payload_length > 0);
    let payload_byte = super::journal::FRAME_HEADER_LEN + payload_length / 2;
    assert!(payload_byte < super::journal::FRAME_HEADER_LEN + payload_length);
    bytes[payload_byte] ^= 0x01;
    std::fs::write(&journal, bytes).unwrap();

    assert!(matches!(
        MissionStore::open(directory.path()),
        Err(super::store::MissionStoreError::Journal(
            super::journal::JournalError::RecordHashMismatch { .. }
        ))
    ));
}

#[test]
fn mission_store_head_detects_loss_of_an_acknowledged_suffix() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    drop(store);

    let journal = directory.path().join("missions/missions.journal.bin");
    std::fs::OpenOptions::new()
        .write(true)
        .open(journal)
        .unwrap()
        .set_len(0)
        .unwrap();

    assert!(MissionStore::open(directory.path())
        .unwrap_err()
        .to_string()
        .contains("ahead of journal"));
}

#[test]
fn mission_store_rejects_a_valid_head_with_the_wrong_hash() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    drop(store);

    let head_path = directory.path().join("missions/missions.head.json");
    let mut head: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&head_path).unwrap()).unwrap();
    head["record_hash"][0] = serde_json::json!(255);
    std::fs::write(&head_path, serde_json::to_vec(&head).unwrap()).unwrap();

    assert!(MissionStore::open(directory.path())
        .unwrap_err()
        .to_string()
        .contains("head hash"));
}

#[test]
fn mission_store_recovers_a_valid_frame_written_ahead_of_the_head() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    let head_path = directory.path().join("missions/missions.head.json");
    let first_head = std::fs::read(&head_path).unwrap();
    store
        .commit(
            "event-2",
            PersistableMissionEvent::StatusChanged {
                mission_id: "mission-1".to_owned(),
                status: MissionStatus::Preparing,
                at_millis: 20,
            },
        )
        .unwrap();
    drop(store);
    std::fs::write(&head_path, first_head).unwrap();

    let recovered = MissionStore::open(directory.path()).unwrap();
    assert_eq!(recovered.last_sequence(), 2);
    let recovered_head: serde_json::Value =
        serde_json::from_slice(&std::fs::read(head_path).unwrap()).unwrap();
    assert_eq!(recovered_head["sequence"], 2);
}

#[cfg(unix)]
#[test]
fn mission_store_uses_private_directory_and_file_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    store.checkpoint().unwrap();

    let mission_directory = directory.path().join("missions");
    let journal = mission_directory.join("missions.journal.bin");
    let head = mission_directory.join("missions.head.json");
    let snapshot = mission_directory.join("missions.snapshot.json");
    assert_eq!(
        std::fs::metadata(mission_directory)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(journal).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(snapshot).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(head).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn handoff_reader_rejects_insecure_store_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit("event-1", persisted_created("mission-1"))
        .unwrap();
    drop(store);
    let mission_directory = directory.path().join("missions");
    let journal = mission_directory.join("missions.journal.bin");

    std::fs::set_permissions(&mission_directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(MissionStoreReader::open_existing(directory.path())
        .unwrap_err()
        .to_string()
        .contains("grants access"));

    std::fs::set_permissions(&mission_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(MissionStoreReader::open_existing(directory.path())
        .unwrap_err()
        .to_string()
        .contains("grants access"));
}

#[test]
fn mission_store_persists_user_spec_but_never_provider_payloads() {
    let sentinel = "PRIVATE_USER_OBJECTIVE_7f44";
    let directory = tempfile::tempdir().unwrap();
    let mut store = MissionStore::open(directory.path()).unwrap();
    store
        .commit(
            "event-1",
            PersistableMissionEvent::mission_created(
                "mission-1",
                "Sensitive mission",
                "/repo",
                sentinel,
                vec!["Explicit user-owned acceptance".to_owned()],
                10,
            )
            .unwrap(),
        )
        .unwrap();
    store.checkpoint().unwrap();

    let mission_directory = directory.path().join("missions");
    let journal = std::fs::read(mission_directory.join("missions.journal.bin")).unwrap();
    let snapshot =
        std::fs::read_to_string(mission_directory.join("missions.snapshot.json")).unwrap();

    assert!(journal
        .windows(sentinel.len())
        .any(|window| window == sentinel.as_bytes()));
    assert!(snapshot.contains(sentinel));
    assert!(!snapshot.contains("raw_provider_transcript"));
}
