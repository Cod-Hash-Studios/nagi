use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

#[cfg(test)]
use std::sync::Mutex;

pub use super::attention::{ResponseFailureCode, ResponseFailureDisposition};

use super::{
    attention::{AttentionDecision, AttentionRisk, AttentionStatus, PaneTarget},
    digest::CanonicalDigest,
    evidence::{CheckDeclaration, EvidenceStatus},
    journal::{
        FramedJournal, JournalError, RecordHash, ReplayCheckpoint, ReplayError, ReplayMode,
        ScanSummary, StateHash, FRAME_VERSION, MAX_JOURNAL_FRAMES, MAX_PAYLOAD_BYTES,
    },
    model::{
        ArchiveProof, MissionDefinition, MissionStatus, ProofReceipt, ProviderKind, ProviderMode,
        ReadyProof,
    },
    proof::ClosurePlan,
};

const STORE_VERSION: u32 = 2;
const STORE_DIRECTORY: &str = "missions";
const JOURNAL_FILE: &str = "missions.journal.bin";
const HEAD_FILE: &str = "missions.head.json";
const SNAPSHOT_FILE: &str = "missions.snapshot.json";
const SNAPSHOT_QUARANTINE_FILE: &str = "missions.snapshot.invalid.json";
const WRITER_LOCK_FILE: &str = "missions.writer.lock";
const MAX_HEAD_BYTES: u64 = 64 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024;

#[cfg(test)]
static FAIL_NEXT_ATOMIC_WRITE_WITH_STORAGE_FULL: Mutex<Option<PathBuf>> = Mutex::new(None);

#[cfg(test)]
fn fail_next_atomic_write_with_storage_full(destination: &Path) {
    *FAIL_NEXT_ATOMIC_WRITE_WITH_STORAGE_FULL
        .lock()
        .expect("storage-full test fault lock should not be poisoned") =
        Some(destination.to_path_buf());
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PersistableMissionEvent {
    MissionCreated {
        mission_id: String,
        title: String,
        repository_path: String,
        repository_hash: String,
        objective: String,
        acceptance_criteria: Vec<String>,
        at_millis: u64,
    },
    ClosureConfigured {
        mission_id: String,
        declarations: Vec<CheckDeclaration>,
        at_millis: u64,
    },
    RunStarted {
        mission_id: String,
        run_id: String,
        provider: ProviderKind,
        mode: ProviderMode,
        worktree_path: String,
        base_revision: String,
        #[serde(default)]
        execute_declared_checks: bool,
        #[serde(default)]
        execute_project_recipe: bool,
        at_millis: u64,
    },
    RunContinued {
        mission_id: String,
        source_run_id: String,
        run_id: String,
        provider: ProviderKind,
        mode: ProviderMode,
        worktree_path: String,
        base_revision: String,
        execute_declared_checks: bool,
        execute_project_recipe: bool,
        handoff_artifact_sha256: String,
        at_millis: u64,
    },
    ProviderSessionBound {
        mission_id: String,
        run_id: String,
        provider_session_id: String,
        at_millis: u64,
    },
    StatusChanged {
        mission_id: String,
        status: MissionStatus,
        at_millis: u64,
    },
    MissionReady {
        mission_id: String,
        receipt: ProofReceipt,
        actor_id: String,
        reason_hash: String,
        at_millis: u64,
    },
    MissionArchived {
        mission_id: String,
        receipt: ProofReceipt,
        ready_seal_digest: String,
        actor_id: String,
        reason_hash: String,
        at_millis: u64,
    },
    WorktreeClaimed {
        mission_id: String,
        relative_path: String,
        at_millis: u64,
    },
    AttentionChanged {
        mission_id: String,
        attention_id: String,
        state: PersistedAttentionState,
        risk: AttentionRisk,
        at_millis: u64,
    },
    EvidenceChanged {
        mission_id: String,
        check_id: String,
        status: EvidenceStatus,
        workspace_hash: String,
        at_millis: u64,
    },
    EvidencePackRecorded {
        mission_id: String,
        run_id: String,
        pack_digest: String,
        workspace_hash: String,
        at_millis: u64,
    },
    ResponseRequested {
        mission_id: String,
        key: ResponseAttemptKey,
        route: PersistedResponseRoute,
        decision: AttentionDecision,
        actor_id: String,
        at_millis: u64,
    },
    ResponseAcknowledged {
        mission_id: String,
        key: ResponseAttemptKey,
        acknowledgement_hash: Option<String>,
        at_millis: u64,
    },
    ResponseFailed {
        mission_id: String,
        key: ResponseAttemptKey,
        disposition: ResponseFailureDisposition,
        code: ResponseFailureCode,
        at_millis: u64,
    },
}

impl PersistableMissionEvent {
    pub(crate) fn proof_lease_digest(&self) -> Option<&str> {
        match self {
            Self::MissionReady { receipt, .. } | Self::MissionArchived { receipt, .. } => {
                Some(receipt.lease_digest())
            }
            _ => None,
        }
    }

    fn validate(&self) -> Result<(), MissionStoreError> {
        let mission_id = match self {
            Self::MissionCreated { mission_id, .. }
            | Self::ClosureConfigured { mission_id, .. }
            | Self::RunStarted { mission_id, .. }
            | Self::RunContinued { mission_id, .. }
            | Self::ProviderSessionBound { mission_id, .. }
            | Self::StatusChanged { mission_id, .. }
            | Self::MissionReady { mission_id, .. }
            | Self::MissionArchived { mission_id, .. }
            | Self::WorktreeClaimed { mission_id, .. }
            | Self::AttentionChanged { mission_id, .. }
            | Self::EvidenceChanged { mission_id, .. }
            | Self::EvidencePackRecorded { mission_id, .. }
            | Self::ResponseRequested { mission_id, .. }
            | Self::ResponseAcknowledged { mission_id, .. }
            | Self::ResponseFailed { mission_id, .. } => mission_id,
        };
        validate_id("mission id", mission_id)?;

        match self {
            Self::MissionCreated {
                title,
                repository_path,
                repository_hash,
                objective,
                acceptance_criteria,
                ..
            } => {
                validate_text("mission title", title, 1, 256)?;
                validate_text("mission repository path", repository_path, 1, 4 * 1024)?;
                if !Path::new(repository_path).is_absolute() {
                    return Err(MissionStoreError::RepositoryPathNotAbsolute);
                }
                if repository_hash != &repository_path_hash(repository_path) {
                    return Err(MissionStoreError::RepositoryHashMismatch);
                }
                validate_text("mission objective", objective, 1, 8 * 1024)?;
                if acceptance_criteria.is_empty() || acceptance_criteria.len() > 16 {
                    return Err(MissionStoreError::InvalidAcceptanceCriteria);
                }
                for criterion in acceptance_criteria {
                    validate_text("acceptance criterion", criterion, 1, 1024)?;
                }
                Ok(())
            }
            Self::RunStarted {
                run_id,
                worktree_path,
                base_revision,
                ..
            } => {
                validate_id("mission run id", run_id)?;
                validate_text("mission worktree path", worktree_path, 1, 4 * 1024)?;
                if !Path::new(worktree_path).is_absolute() {
                    return Err(MissionStoreError::WorktreePathNotAbsolute);
                }
                validate_revision(base_revision)
            }
            Self::RunContinued {
                source_run_id,
                run_id,
                worktree_path,
                base_revision,
                handoff_artifact_sha256,
                ..
            } => {
                validate_id("source mission run id", source_run_id)?;
                validate_id("mission run id", run_id)?;
                validate_text("mission worktree path", worktree_path, 1, 4 * 1024)?;
                if !Path::new(worktree_path).is_absolute() {
                    return Err(MissionStoreError::WorktreePathNotAbsolute);
                }
                validate_revision(base_revision)?;
                validate_hash("handoff artifact sha256", handoff_artifact_sha256)
            }
            Self::ClosureConfigured { declarations, .. } => {
                if declarations.is_empty() || declarations.len() > 32 {
                    return Err(MissionStoreError::InvalidClosurePlan);
                }
                for declaration in declarations {
                    declaration
                        .validate_persisted()
                        .map_err(|_| MissionStoreError::InvalidClosurePlan)?;
                }
                Ok(())
            }
            Self::ProviderSessionBound {
                run_id,
                provider_session_id,
                ..
            } => {
                validate_id("mission run id", run_id)?;
                validate_opaque_id("provider session id", provider_session_id)
            }
            Self::WorktreeClaimed { relative_path, .. } => validate_relative_path(relative_path),
            Self::AttentionChanged { attention_id, .. } => {
                validate_id("attention id", attention_id)
            }
            Self::EvidenceChanged {
                check_id,
                workspace_hash,
                ..
            } => {
                validate_id("check id", check_id)?;
                validate_hash("workspace hash", workspace_hash)
            }
            Self::EvidencePackRecorded {
                run_id,
                pack_digest,
                workspace_hash,
                ..
            } => {
                validate_id("mission run id", run_id)?;
                validate_hash("evidence pack digest", pack_digest)?;
                validate_hash("workspace hash", workspace_hash)
            }
            Self::StatusChanged { .. } => Ok(()),
            Self::MissionReady {
                receipt,
                actor_id,
                reason_hash,
                ..
            } => {
                validate_proof_receipt(receipt)?;
                validate_id("proof actor id", actor_id)?;
                validate_hash("proof reason hash", reason_hash)
            }
            Self::MissionArchived {
                receipt,
                ready_seal_digest,
                actor_id,
                reason_hash,
                ..
            } => {
                validate_proof_receipt(receipt)?;
                validate_hash("ready proof seal", ready_seal_digest)?;
                validate_id("proof actor id", actor_id)?;
                validate_hash("proof reason hash", reason_hash)
            }
            Self::ResponseRequested {
                key,
                route,
                actor_id,
                ..
            } => {
                key.validate()?;
                route.validate()?;
                validate_id("response actor id", actor_id)
            }
            Self::ResponseAcknowledged {
                key,
                acknowledgement_hash,
                ..
            } => {
                key.validate()?;
                if let Some(hash) = acknowledgement_hash {
                    validate_hash("provider acknowledgement hash", hash)?;
                }
                Ok(())
            }
            Self::ResponseFailed { key, .. } => key.validate(),
        }
    }
}

impl PersistableMissionEvent {
    pub(crate) fn mission_created(
        mission_id: impl Into<String>,
        title: impl Into<String>,
        repository_path: impl Into<String>,
        objective: impl Into<String>,
        acceptance_criteria: Vec<String>,
        at_millis: u64,
    ) -> Result<Self, MissionStoreError> {
        let repository_path = repository_path.into();
        let event = Self::MissionCreated {
            mission_id: mission_id.into(),
            title: title.into(),
            repository_hash: repository_path_hash(&repository_path),
            repository_path,
            objective: objective.into(),
            acceptance_criteria,
            at_millis,
        };
        event.validate()?;
        Ok(event)
    }

    #[cfg(test)]
    pub(crate) fn run_started(
        mission_id: impl Into<String>,
        run_id: impl Into<String>,
        provider: ProviderKind,
        mode: ProviderMode,
        worktree_path: impl Into<String>,
        base_revision: impl Into<String>,
        at_millis: u64,
    ) -> Result<Self, MissionStoreError> {
        Self::run_started_with_check_execution(
            mission_id,
            run_id,
            provider,
            mode,
            worktree_path,
            base_revision,
            false,
            false,
            at_millis,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_started_with_check_execution(
        mission_id: impl Into<String>,
        run_id: impl Into<String>,
        provider: ProviderKind,
        mode: ProviderMode,
        worktree_path: impl Into<String>,
        base_revision: impl Into<String>,
        execute_declared_checks: bool,
        execute_project_recipe: bool,
        at_millis: u64,
    ) -> Result<Self, MissionStoreError> {
        let event = Self::RunStarted {
            mission_id: mission_id.into(),
            run_id: run_id.into(),
            provider,
            mode,
            worktree_path: worktree_path.into(),
            base_revision: base_revision.into(),
            execute_declared_checks,
            execute_project_recipe,
            at_millis,
        };
        event.validate()?;
        Ok(event)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_continued(
        mission_id: impl Into<String>,
        source_run_id: impl Into<String>,
        run_id: impl Into<String>,
        provider: ProviderKind,
        mode: ProviderMode,
        worktree_path: impl Into<String>,
        base_revision: impl Into<String>,
        execute_declared_checks: bool,
        execute_project_recipe: bool,
        handoff_artifact_sha256: impl Into<String>,
        at_millis: u64,
    ) -> Result<Self, MissionStoreError> {
        let event = Self::RunContinued {
            mission_id: mission_id.into(),
            source_run_id: source_run_id.into(),
            run_id: run_id.into(),
            provider,
            mode,
            worktree_path: worktree_path.into(),
            base_revision: base_revision.into(),
            execute_declared_checks,
            execute_project_recipe,
            handoff_artifact_sha256: handoff_artifact_sha256.into(),
            at_millis,
        };
        event.validate()?;
        Ok(event)
    }

    pub(crate) fn closure_configured(
        mission_id: impl Into<String>,
        declarations: Vec<CheckDeclaration>,
        at_millis: u64,
    ) -> Result<Self, MissionStoreError> {
        let event = Self::ClosureConfigured {
            mission_id: mission_id.into(),
            declarations,
            at_millis,
        };
        event.validate()?;
        Ok(event)
    }

    pub(crate) fn provider_session_bound(
        mission_id: impl Into<String>,
        run_id: impl Into<String>,
        provider_session_id: impl Into<String>,
        at_millis: u64,
    ) -> Result<Self, MissionStoreError> {
        let event = Self::ProviderSessionBound {
            mission_id: mission_id.into(),
            run_id: run_id.into(),
            provider_session_id: provider_session_id.into(),
            at_millis,
        };
        event.validate()?;
        Ok(event)
    }

    #[allow(
        dead_code,
        reason = "sealed ready events are staged until public mission closure"
    )]
    pub(crate) fn mission_ready(
        mission_id: impl Into<String>,
        proof: ReadyProof,
        actor_id: impl Into<String>,
        reason_hash: impl Into<String>,
        at_millis: u64,
    ) -> Result<Self, MissionStoreError> {
        let mission_id = mission_id.into();
        let (proof_mission_id, receipt) = proof.into_receipt();
        if proof_mission_id != mission_id {
            return Err(MissionStoreError::ProofMissionMismatch);
        }
        let event = Self::MissionReady {
            mission_id,
            receipt,
            actor_id: actor_id.into(),
            reason_hash: reason_hash.into(),
            at_millis,
        };
        event.validate()?;
        Ok(event)
    }

    #[allow(
        dead_code,
        reason = "sealed archive events are staged until public mission closure"
    )]
    pub(crate) fn mission_archived(
        mission_id: impl Into<String>,
        proof: ArchiveProof,
        actor_id: impl Into<String>,
        reason_hash: impl Into<String>,
        at_millis: u64,
    ) -> Result<Self, MissionStoreError> {
        let mission_id = mission_id.into();
        let (proof_mission_id, ready_seal_digest, receipt) = proof.into_receipt();
        if proof_mission_id != mission_id {
            return Err(MissionStoreError::ProofMissionMismatch);
        }
        let event = Self::MissionArchived {
            mission_id,
            receipt,
            ready_seal_digest,
            actor_id: actor_id.into(),
            reason_hash: reason_hash.into(),
            at_millis,
        };
        event.validate()?;
        Ok(event)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ResponseAttemptKey {
    attention_id: String,
    request_generation: u64,
    attempt: u32,
}

impl ResponseAttemptKey {
    #[allow(
        dead_code,
        reason = "response attempts stay private until provider replies are public"
    )]
    pub fn new(
        attention_id: impl Into<String>,
        request_generation: u64,
        attempt: u32,
    ) -> Result<Self, MissionStoreError> {
        let key = Self {
            attention_id: attention_id.into(),
            request_generation,
            attempt,
        };
        key.validate()?;
        Ok(key)
    }

    fn validate(&self) -> Result<(), MissionStoreError> {
        validate_id("attention id", &self.attention_id)?;
        if self.request_generation == 0 || self.attempt == 0 {
            return Err(MissionStoreError::InvalidResponseAttempt);
        }
        Ok(())
    }

    fn digest(&self) -> String {
        let mut digest = CanonicalDigest::new(b"persisted-response-attempt-v1");
        digest.string(&self.attention_id);
        digest.u64(self.request_generation);
        digest.u64(u64::from(self.attempt));
        digest.finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PersistedResponseRoute {
    provider: ProviderKind,
    mission_run_id: String,
    session_id: String,
    #[serde(default)]
    pane_target: Option<PaneTarget>,
    provider_request_id: String,
}

impl PersistedResponseRoute {
    #[must_use]
    #[allow(
        dead_code,
        reason = "pane-routed replies stay private until provider replies are public"
    )]
    pub fn new(
        provider: ProviderKind,
        mission_run_id: impl Into<String>,
        session_id: impl Into<String>,
        pane_target: PaneTarget,
        provider_request_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            mission_run_id: mission_run_id.into(),
            session_id: session_id.into(),
            pane_target: Some(pane_target),
            provider_request_id: provider_request_id.into(),
        }
    }

    #[must_use]
    #[allow(
        dead_code,
        reason = "managed replies stay private until provider replies are public"
    )]
    pub fn managed(
        provider: ProviderKind,
        mission_run_id: impl Into<String>,
        session_id: impl Into<String>,
        provider_request_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            mission_run_id: mission_run_id.into(),
            session_id: session_id.into(),
            pane_target: None,
            provider_request_id: provider_request_id.into(),
        }
    }

    fn validate(&self) -> Result<(), MissionStoreError> {
        validate_id("mission run id", &self.mission_run_id)?;
        validate_opaque_id("provider session id", &self.session_id)?;
        if let Some(pane_target) = &self.pane_target {
            validate_id("workspace id", pane_target.workspace())?;
            validate_id("pane id", pane_target.pane())?;
        }
        validate_opaque_id("provider request id", &self.provider_request_id)
    }

    fn validate_for_run(&self, run: &PersistedRun) -> Result<(), MissionStoreError> {
        let mode_matches_route = match run.mode {
            ProviderMode::Managed => self.pane_target.is_none(),
            ProviderMode::Passthrough => self.pane_target.is_some(),
        };
        if self.provider != run.provider
            || self.mission_run_id != run.run_id
            || run.provider_session_id.as_deref() != Some(self.session_id.as_str())
            || !mode_matches_route
        {
            return Err(MissionStoreError::ResponseRouteMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedAttentionState {
    Open,
    PendingResponse,
    ReconciliationRequired,
    Resolved,
    Dismissed,
    Expired,
}

impl From<&AttentionStatus> for PersistedAttentionState {
    fn from(status: &AttentionStatus) -> Self {
        match status {
            AttentionStatus::Open => Self::Open,
            AttentionStatus::PendingResponse { .. } => Self::PendingResponse,
            AttentionStatus::ReconciliationRequired { .. } => Self::ReconciliationRequired,
            AttentionStatus::Resolved { .. } => Self::Resolved,
            AttentionStatus::Dismissed { .. } => Self::Dismissed,
            AttentionStatus::Expired { .. } => Self::Expired,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SequencedMissionEvent {
    version: u32,
    sequence: u64,
    event_id: String,
    previous_hash: RecordHash,
    projection_digest_after: StateHash,
    event: PersistableMissionEvent,
}

impl SequencedMissionEvent {
    #[must_use]
    #[allow(
        dead_code,
        reason = "raw event sequence inspection is retained for staged closure auditing"
    )]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedAttention {
    state: PersistedAttentionState,
    risk: AttentionRisk,
    updated_at_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedEvidence {
    status: EvidenceStatus,
    workspace_hash: String,
    updated_at_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedEvidencePack {
    run_id: String,
    pack_digest: String,
    workspace_hash: String,
    recorded_at_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedResponseAttempt {
    key: ResponseAttemptKey,
    route: PersistedResponseRoute,
    decision: AttentionDecision,
    actor_id: String,
    state: PersistedResponseState,
    updated_at_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PersistedResponseState {
    Requested,
    Acknowledged {
        acknowledgement_hash: Option<String>,
    },
    Failed {
        code: ResponseFailureCode,
    },
    ReconciliationRequired {
        code: ResponseFailureCode,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedMission {
    title: String,
    repository_path: String,
    repository_hash: String,
    objective: String,
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    check_declarations: Vec<CheckDeclaration>,
    status: MissionStatus,
    #[serde(default)]
    run: Option<PersistedRun>,
    #[serde(default)]
    run_history: Vec<PersistedRun>,
    worktree_relative_path: Option<String>,
    attention: BTreeMap<String, PersistedAttention>,
    evidence: BTreeMap<String, PersistedEvidence>,
    #[serde(default)]
    latest_evidence_pack: Option<PersistedEvidencePack>,
    responses: BTreeMap<String, PersistedResponseAttempt>,
    ready_receipt: Option<ProofReceipt>,
    archive_receipt: Option<ProofReceipt>,
    updated_at_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedRun {
    run_id: String,
    provider: ProviderKind,
    mode: ProviderMode,
    worktree_path: String,
    base_revision: String,
    #[serde(default)]
    execute_declared_checks: bool,
    #[serde(default)]
    execute_project_recipe: bool,
    #[serde(default)]
    handoff_from_run_id: Option<String>,
    #[serde(default)]
    handoff_artifact_sha256: Option<String>,
    provider_session_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MissionProjection {
    missions: BTreeMap<String, PersistedMission>,
}

impl MissionProjection {
    fn apply(&mut self, event: &PersistableMissionEvent) -> Result<(), MissionStoreError> {
        match event {
            PersistableMissionEvent::MissionCreated {
                mission_id,
                title,
                repository_path,
                repository_hash,
                objective,
                acceptance_criteria,
                at_millis,
            } => {
                if self.missions.contains_key(mission_id) {
                    return Err(MissionStoreError::MissionAlreadyExists(mission_id.clone()));
                }
                self.missions.insert(
                    mission_id.clone(),
                    PersistedMission {
                        title: title.clone(),
                        repository_path: repository_path.clone(),
                        repository_hash: repository_hash.clone(),
                        objective: objective.clone(),
                        acceptance_criteria: acceptance_criteria.clone(),
                        check_declarations: Vec::new(),
                        status: MissionStatus::Draft,
                        run: None,
                        run_history: Vec::new(),
                        worktree_relative_path: None,
                        attention: BTreeMap::new(),
                        evidence: BTreeMap::new(),
                        latest_evidence_pack: None,
                        responses: BTreeMap::new(),
                        ready_receipt: None,
                        archive_receipt: None,
                        updated_at_millis: *at_millis,
                    },
                );
            }
            PersistableMissionEvent::ClosureConfigured {
                mission_id,
                declarations,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                if mission.status != MissionStatus::Draft || mission.run.is_some() {
                    return Err(MissionStoreError::ClosureCannotBeChanged);
                }
                if !mission.check_declarations.is_empty() {
                    return Err(MissionStoreError::ClosureAlreadyConfigured);
                }
                let definition = MissionDefinition::new(
                    mission_id,
                    &mission.title,
                    &mission.repository_path,
                    &mission.objective,
                    mission.acceptance_criteria.clone(),
                )
                .map_err(|_| MissionStoreError::InvalidClosurePlan)?;
                ClosurePlan::new(&definition.acceptance_criterion_ids(), declarations)
                    .map_err(|_| MissionStoreError::InvalidClosurePlan)?;
                mission.check_declarations = declarations.clone();
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::RunStarted {
                mission_id,
                run_id,
                provider,
                mode,
                worktree_path,
                base_revision,
                execute_declared_checks,
                execute_project_recipe,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                if mission.status != MissionStatus::Draft || mission.run.is_some() {
                    return Err(MissionStoreError::RunAlreadyStarted);
                }
                if mission.check_declarations.is_empty() {
                    return Err(MissionStoreError::ClosureMissing);
                }
                mission.run = Some(PersistedRun {
                    run_id: run_id.clone(),
                    provider: *provider,
                    mode: *mode,
                    worktree_path: worktree_path.clone(),
                    base_revision: base_revision.clone(),
                    execute_declared_checks: *execute_declared_checks,
                    execute_project_recipe: *execute_project_recipe,
                    handoff_from_run_id: None,
                    handoff_artifact_sha256: None,
                    provider_session_id: None,
                });
                mission.status = MissionStatus::Preparing;
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::RunContinued {
                mission_id,
                source_run_id,
                run_id,
                provider,
                mode,
                worktree_path,
                base_revision,
                execute_declared_checks,
                execute_project_recipe,
                handoff_artifact_sha256,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                if !matches!(
                    mission.status,
                    MissionStatus::Blocked
                        | MissionStatus::Failed
                        | MissionStatus::ReviewRequired
                        | MissionStatus::ReadyToClose
                ) {
                    return Err(MissionStoreError::InvalidStatusTransition {
                        from: mission.status,
                        to: MissionStatus::Preparing,
                    });
                }
                if mission.attention.values().any(|attention| {
                    matches!(
                        attention.state,
                        PersistedAttentionState::Open
                            | PersistedAttentionState::PendingResponse
                            | PersistedAttentionState::ReconciliationRequired
                    )
                }) {
                    return Err(MissionStoreError::HandoffAttentionUnresolved);
                }
                let source = mission.run.as_ref().ok_or(MissionStoreError::RunMissing)?;
                if source.run_id != *source_run_id
                    || source.worktree_path != *worktree_path
                    || source.base_revision != *base_revision
                    || source.execute_declared_checks != *execute_declared_checks
                    || source.execute_project_recipe != *execute_project_recipe
                {
                    return Err(MissionStoreError::RunMismatch);
                }
                if source.provider == *provider {
                    return Err(MissionStoreError::HandoffSameProvider);
                }
                if source.run_id == *run_id
                    || mission
                        .run_history
                        .iter()
                        .any(|previous| previous.run_id == *run_id)
                {
                    return Err(MissionStoreError::RunAlreadyExists);
                }
                let source = mission.run.take().ok_or(MissionStoreError::RunMissing)?;
                mission.run_history.push(source);
                mission.run = Some(PersistedRun {
                    run_id: run_id.clone(),
                    provider: *provider,
                    mode: *mode,
                    worktree_path: worktree_path.clone(),
                    base_revision: base_revision.clone(),
                    execute_declared_checks: *execute_declared_checks,
                    execute_project_recipe: *execute_project_recipe,
                    handoff_from_run_id: Some(source_run_id.clone()),
                    handoff_artifact_sha256: Some(handoff_artifact_sha256.clone()),
                    provider_session_id: None,
                });
                for evidence in mission.evidence.values_mut() {
                    evidence.status = EvidenceStatus::Stale;
                    evidence.updated_at_millis = logical_at_millis;
                }
                mission.latest_evidence_pack = None;
                mission.ready_receipt = None;
                mission.archive_receipt = None;
                mission.status = MissionStatus::Preparing;
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::ProviderSessionBound {
                mission_id,
                run_id,
                provider_session_id,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                if mission.status != MissionStatus::Preparing {
                    return Err(MissionStoreError::InvalidStatusTransition {
                        from: mission.status,
                        to: MissionStatus::Active,
                    });
                }
                let run = mission.run.as_mut().ok_or(MissionStoreError::RunMissing)?;
                if run.run_id != *run_id {
                    return Err(MissionStoreError::RunMismatch);
                }
                run.provider_session_id = Some(provider_session_id.clone());
                mission.status = MissionStatus::Active;
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::StatusChanged {
                mission_id,
                status,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                if matches!(
                    status,
                    MissionStatus::ReadyToClose | MissionStatus::Archived
                ) {
                    return Err(MissionStoreError::SealedStatusRequiresProof);
                }
                if !status.can_persist_from(mission.status) {
                    return Err(MissionStoreError::InvalidStatusTransition {
                        from: mission.status,
                        to: *status,
                    });
                }
                mission.status = *status;
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::MissionReady {
                mission_id,
                receipt,
                at_millis,
                ..
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                if mission.status != MissionStatus::ReviewRequired {
                    return Err(MissionStoreError::InvalidStatusTransition {
                        from: mission.status,
                        to: MissionStatus::ReadyToClose,
                    });
                }
                if receipt.verified_at_millis() > logical_at_millis {
                    return Err(MissionStoreError::ProofFromFuture);
                }
                mission.status = MissionStatus::ReadyToClose;
                mission.ready_receipt = Some(receipt.clone());
                mission.archive_receipt = None;
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::MissionArchived {
                mission_id,
                receipt,
                ready_seal_digest,
                at_millis,
                ..
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                if mission.status != MissionStatus::ReadyToClose {
                    return Err(MissionStoreError::InvalidStatusTransition {
                        from: mission.status,
                        to: MissionStatus::Archived,
                    });
                }
                let ready = mission
                    .ready_receipt
                    .as_ref()
                    .ok_or(MissionStoreError::ReadyReceiptMissing)?;
                if ready.seal_digest() != ready_seal_digest
                    || ready.subject_digest() != receipt.subject_digest()
                {
                    return Err(MissionStoreError::ProofBasisChanged);
                }
                if receipt.authority_sequence() <= ready.authority_sequence()
                    || receipt.verified_at_millis() < ready.verified_at_millis()
                    || receipt.verified_at_millis() > logical_at_millis
                {
                    return Err(MissionStoreError::FreshArchiveProofRequired);
                }
                mission.status = MissionStatus::Archived;
                mission.archive_receipt = Some(receipt.clone());
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::WorktreeClaimed {
                mission_id,
                relative_path,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                mission.worktree_relative_path = Some(relative_path.clone());
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::AttentionChanged {
                mission_id,
                attention_id,
                state,
                risk,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                match (mission.attention.get_mut(attention_id), state) {
                    (None, PersistedAttentionState::Open) => {
                        mission.attention.insert(
                            attention_id.clone(),
                            PersistedAttention {
                                state: *state,
                                risk: *risk,
                                updated_at_millis: logical_at_millis,
                            },
                        );
                    }
                    (Some(attention), PersistedAttentionState::Expired)
                        if attention.state == PersistedAttentionState::Open
                            && attention.risk == *risk =>
                    {
                        attention.state = PersistedAttentionState::Expired;
                        attention.updated_at_millis = logical_at_millis;
                    }
                    _ => return Err(MissionStoreError::SealedAttentionState),
                }
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::EvidenceChanged {
                mission_id,
                check_id,
                status,
                workspace_hash,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                mission.evidence.insert(
                    check_id.clone(),
                    PersistedEvidence {
                        status: *status,
                        workspace_hash: workspace_hash.clone(),
                        updated_at_millis: logical_at_millis,
                    },
                );
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::EvidencePackRecorded {
                mission_id,
                run_id,
                pack_digest,
                workspace_hash,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                let run = mission.run.as_ref().ok_or(MissionStoreError::RunMissing)?;
                if run.run_id != *run_id {
                    return Err(MissionStoreError::RunMismatch);
                }
                if !matches!(
                    mission.status,
                    MissionStatus::ReviewRequired | MissionStatus::ReadyToClose
                ) {
                    return Err(MissionStoreError::InvalidStatusTransition {
                        from: mission.status,
                        to: MissionStatus::ReviewRequired,
                    });
                }
                mission.latest_evidence_pack = Some(PersistedEvidencePack {
                    run_id: run_id.clone(),
                    pack_digest: pack_digest.clone(),
                    workspace_hash: workspace_hash.clone(),
                    recorded_at_millis: logical_at_millis,
                });
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::ResponseRequested {
                mission_id,
                key,
                route,
                decision,
                actor_id,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                route
                    .validate_for_run(mission.run.as_ref().ok_or(MissionStoreError::RunMissing)?)?;
                let attention = mission
                    .attention
                    .get_mut(&key.attention_id)
                    .ok_or_else(|| {
                        MissionStoreError::AttentionNotFound(key.attention_id.clone())
                    })?;
                if attention.state != PersistedAttentionState::Open {
                    return Err(MissionStoreError::InvalidResponseState);
                }
                let response_id = key.digest();
                if mission.responses.contains_key(&response_id) {
                    return Err(MissionStoreError::ResponseAttemptAlreadyExists);
                }
                let expected_attempt = mission
                    .responses
                    .values()
                    .filter(|response| {
                        response.key.attention_id == key.attention_id
                            && response.key.request_generation == key.request_generation
                    })
                    .map(|response| response.key.attempt)
                    .max()
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or(MissionStoreError::InvalidResponseAttempt)?;
                if key.attempt != expected_attempt {
                    return Err(MissionStoreError::InvalidResponseAttempt);
                }
                attention.state = PersistedAttentionState::PendingResponse;
                attention.updated_at_millis = logical_at_millis;
                mission.responses.insert(
                    response_id,
                    PersistedResponseAttempt {
                        key: key.clone(),
                        route: route.clone(),
                        decision: *decision,
                        actor_id: actor_id.clone(),
                        state: PersistedResponseState::Requested,
                        updated_at_millis: logical_at_millis,
                    },
                );
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::ResponseAcknowledged {
                mission_id,
                key,
                acknowledgement_hash,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                let response = mission
                    .responses
                    .get_mut(&key.digest())
                    .filter(|response| response.key == *key)
                    .ok_or(MissionStoreError::ResponseAttemptNotFound)?;
                if !matches!(
                    response.state,
                    PersistedResponseState::Requested
                        | PersistedResponseState::ReconciliationRequired { .. }
                ) {
                    return Err(MissionStoreError::InvalidResponseState);
                }
                response.state = PersistedResponseState::Acknowledged {
                    acknowledgement_hash: acknowledgement_hash.clone(),
                };
                response.updated_at_millis = logical_at_millis;
                let attention = mission
                    .attention
                    .get_mut(&key.attention_id)
                    .ok_or_else(|| {
                        MissionStoreError::AttentionNotFound(key.attention_id.clone())
                    })?;
                attention.state = PersistedAttentionState::Resolved;
                attention.updated_at_millis = logical_at_millis;
                mission.updated_at_millis = logical_at_millis;
            }
            PersistableMissionEvent::ResponseFailed {
                mission_id,
                key,
                disposition,
                code,
                at_millis,
            } => {
                let mission = self.mission_mut_at(mission_id, *at_millis)?;
                let logical_at_millis = (*at_millis).max(mission.updated_at_millis);
                let response = mission
                    .responses
                    .get_mut(&key.digest())
                    .filter(|response| response.key == *key)
                    .ok_or(MissionStoreError::ResponseAttemptNotFound)?;
                if response.state != PersistedResponseState::Requested {
                    return Err(MissionStoreError::InvalidResponseState);
                }
                let (response_state, attention_state) = match disposition {
                    ResponseFailureDisposition::DefinitelyNotApplied => (
                        PersistedResponseState::Failed { code: *code },
                        PersistedAttentionState::Open,
                    ),
                    ResponseFailureDisposition::DeliveryUnknown => (
                        PersistedResponseState::ReconciliationRequired { code: *code },
                        PersistedAttentionState::ReconciliationRequired,
                    ),
                };
                response.state = response_state;
                response.updated_at_millis = logical_at_millis;
                let attention = mission
                    .attention
                    .get_mut(&key.attention_id)
                    .ok_or_else(|| {
                        MissionStoreError::AttentionNotFound(key.attention_id.clone())
                    })?;
                attention.state = attention_state;
                attention.updated_at_millis = logical_at_millis;
                mission.updated_at_millis = logical_at_millis;
            }
        }
        Ok(())
    }

    fn mission_mut(
        &mut self,
        mission_id: &str,
    ) -> Result<&mut PersistedMission, MissionStoreError> {
        self.missions
            .get_mut(mission_id)
            .ok_or_else(|| MissionStoreError::MissionNotFound(mission_id.to_owned()))
    }

    fn mission_mut_at(
        &mut self,
        mission_id: &str,
        _at_millis: u64,
    ) -> Result<&mut PersistedMission, MissionStoreError> {
        self.mission_mut(mission_id)
    }

    #[must_use]
    #[allow(
        dead_code,
        reason = "status projection inspection is retained for staged lifecycle tests"
    )]
    pub fn mission_status(&self, mission_id: &str) -> Option<MissionStatus> {
        self.missions.get(mission_id).map(|mission| mission.status)
    }

    #[must_use]
    #[allow(
        dead_code,
        reason = "response projection inspection is staged until provider replies are public"
    )]
    pub fn response_state(
        &self,
        mission_id: &str,
        key: &ResponseAttemptKey,
    ) -> Option<&PersistedResponseState> {
        self.missions
            .get(mission_id)?
            .responses
            .get(&key.digest())
            .filter(|response| response.key == *key)
            .map(|response| &response.state)
    }

    #[allow(
        dead_code,
        reason = "response attempt inspection is staged until provider replies are public"
    )]
    pub(crate) fn next_response_attempt(
        &self,
        mission_id: &str,
        attention_id: &str,
        request_generation: u64,
    ) -> Result<u32, MissionStoreError> {
        let mission = self
            .missions
            .get(mission_id)
            .ok_or_else(|| MissionStoreError::MissionNotFound(mission_id.to_owned()))?;
        let attention = mission
            .attention
            .get(attention_id)
            .ok_or_else(|| MissionStoreError::AttentionNotFound(attention_id.to_owned()))?;
        if attention.state != PersistedAttentionState::Open || request_generation == 0 {
            return Err(MissionStoreError::InvalidResponseState);
        }
        mission
            .responses
            .values()
            .filter(|response| {
                response.key.attention_id == attention_id
                    && response.key.request_generation == request_generation
            })
            .map(|response| response.key.attempt)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(MissionStoreError::InvalidResponseAttempt)
    }

    #[must_use]
    #[allow(
        dead_code,
        reason = "attention projection inspection is staged until the mission cockpit is public"
    )]
    pub fn attention_state(
        &self,
        mission_id: &str,
        attention_id: &str,
    ) -> Option<PersistedAttentionState> {
        self.missions
            .get(mission_id)?
            .attention
            .get(attention_id)
            .map(|attention| attention.state)
    }

    pub(crate) fn mission_view(&self, mission_id: &str) -> Option<MissionView> {
        let mission = self.missions.get(mission_id)?;
        Some(MissionView::from_persisted(mission_id, mission))
    }

    pub(crate) fn mission_views(&self) -> Vec<MissionView> {
        self.missions
            .iter()
            .map(|(mission_id, mission)| MissionView::from_persisted(mission_id, mission))
            .collect()
    }

    pub(crate) fn attention_views(&self) -> Vec<DurableAttentionView> {
        let mut views = self
            .missions
            .iter()
            .flat_map(|(mission_id, mission)| {
                mission
                    .attention
                    .iter()
                    .map(move |(attention_id, attention)| {
                        let response = mission
                            .responses
                            .values()
                            .filter(|response| response.key.attention_id == *attention_id)
                            .max_by_key(|response| response.key.attempt)
                            .map(|response| DurableResponseView {
                                decision: response.decision,
                                actor_id: response.actor_id.clone(),
                                state: response.state.clone(),
                                updated_at_millis: response.updated_at_millis,
                                attempt: response.key.attempt,
                            });
                        DurableAttentionView {
                            mission_id: mission_id.clone(),
                            attention_id: attention_id.clone(),
                            state: attention.state,
                            risk: attention.risk,
                            updated_at_millis: attention.updated_at_millis,
                            response,
                        }
                    })
            })
            .collect::<Vec<_>>();
        views.sort_by(|left, right| {
            left.mission_id
                .cmp(&right.mission_id)
                .then_with(|| left.attention_id.cmp(&right.attention_id))
        });
        views
    }

    pub(crate) fn unresolved_attention_ids(
        &self,
        mission_id: &str,
    ) -> Result<BTreeSet<String>, MissionStoreError> {
        let mission = self
            .missions
            .get(mission_id)
            .ok_or_else(|| MissionStoreError::MissionNotFound(mission_id.to_owned()))?;
        Ok(mission
            .attention
            .iter()
            .filter(|(_, attention)| {
                matches!(
                    attention.state,
                    PersistedAttentionState::Open
                        | PersistedAttentionState::PendingResponse
                        | PersistedAttentionState::ReconciliationRequired
                )
            })
            .map(|(attention_id, _)| attention_id.clone())
            .collect())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MissionView {
    pub(crate) mission_id: String,
    pub(crate) title: String,
    pub(crate) repository_path: String,
    pub(crate) repository_hash: String,
    pub(crate) objective: String,
    pub(crate) acceptance_criteria: Vec<String>,
    pub(crate) check_declarations: Vec<CheckDeclaration>,
    pub(crate) status: MissionStatus,
    pub(crate) run: Option<MissionRunView>,
    pub(crate) run_history: Vec<MissionRunView>,
    pub(crate) unresolved_attention_count: usize,
    pub(crate) latest_evidence_pack_digest: Option<String>,
    pub(crate) ready_receipt: Option<ProofReceipt>,
    pub(crate) archive_receipt: Option<ProofReceipt>,
    pub(crate) evidence: Vec<MissionEvidenceView>,
    pub(crate) updated_at_millis: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DurableAttentionView {
    pub(crate) mission_id: String,
    pub(crate) attention_id: String,
    pub(crate) state: PersistedAttentionState,
    pub(crate) risk: AttentionRisk,
    pub(crate) updated_at_millis: u64,
    pub(crate) response: Option<DurableResponseView>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DurableResponseView {
    pub(crate) decision: AttentionDecision,
    pub(crate) actor_id: String,
    pub(crate) state: PersistedResponseState,
    pub(crate) updated_at_millis: u64,
    pub(crate) attempt: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MissionEvidenceView {
    pub(crate) check_id: String,
    pub(crate) status: EvidenceStatus,
    pub(crate) workspace_hash: String,
    pub(crate) updated_at_millis: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MissionRunView {
    pub(crate) run_id: String,
    pub(crate) provider: ProviderKind,
    pub(crate) mode: ProviderMode,
    pub(crate) worktree_path: String,
    pub(crate) base_revision: String,
    pub(crate) provider_session_id: Option<String>,
    pub(crate) execute_declared_checks: bool,
    pub(crate) execute_project_recipe: bool,
    pub(crate) handoff_from_run_id: Option<String>,
    pub(crate) handoff_artifact_sha256: Option<String>,
}

impl MissionView {
    fn from_persisted(mission_id: &str, mission: &PersistedMission) -> Self {
        let unresolved_attention_count = mission
            .attention
            .values()
            .filter(|attention| {
                matches!(
                    attention.state,
                    PersistedAttentionState::Open
                        | PersistedAttentionState::PendingResponse
                        | PersistedAttentionState::ReconciliationRequired
                )
            })
            .count();
        Self {
            mission_id: mission_id.to_owned(),
            title: mission.title.clone(),
            repository_path: mission.repository_path.clone(),
            repository_hash: mission.repository_hash.clone(),
            objective: mission.objective.clone(),
            acceptance_criteria: mission.acceptance_criteria.clone(),
            check_declarations: mission.check_declarations.clone(),
            status: mission.status,
            run: mission.run.as_ref().map(mission_run_view),
            run_history: mission.run_history.iter().map(mission_run_view).collect(),
            unresolved_attention_count,
            latest_evidence_pack_digest: mission
                .latest_evidence_pack
                .as_ref()
                .map(|pack| pack.pack_digest.clone()),
            ready_receipt: mission.ready_receipt.clone(),
            archive_receipt: mission.archive_receipt.clone(),
            evidence: mission
                .evidence
                .iter()
                .map(|(check_id, evidence)| MissionEvidenceView {
                    check_id: check_id.clone(),
                    status: evidence.status,
                    workspace_hash: evidence.workspace_hash.clone(),
                    updated_at_millis: evidence.updated_at_millis,
                })
                .collect(),
            updated_at_millis: mission.updated_at_millis,
        }
    }
}

fn mission_run_view(run: &PersistedRun) -> MissionRunView {
    MissionRunView {
        run_id: run.run_id.clone(),
        provider: run.provider,
        mode: run.mode,
        worktree_path: run.worktree_path.clone(),
        base_revision: run.base_revision.clone(),
        provider_session_id: run.provider_session_id.clone(),
        execute_declared_checks: run.execute_declared_checks,
        execute_project_recipe: run.execute_project_recipe,
        handoff_from_run_id: run.handoff_from_run_id.clone(),
        handoff_artifact_sha256: run.handoff_artifact_sha256.clone(),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct MissionSnapshot {
    version: u32,
    last_sequence: u64,
    last_record_hash: RecordHash,
    journal_offset: u64,
    projection: MissionProjection,
    event_index: BTreeMap<String, EventIndexEntry>,
    state_digest: StateHash,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct EventIndexEntry {
    sequence: u64,
    event_fingerprint: Box<str>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct JournalHead {
    version: u32,
    sequence: u64,
    record_hash: RecordHash,
}

impl JournalHead {
    const EMPTY: Self = Self {
        version: STORE_VERSION,
        sequence: 0,
        record_hash: RecordHash::ZERO,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitOutcome {
    sequence: u64,
    was_duplicate: bool,
}

impl CommitOutcome {
    #[must_use]
    #[allow(
        dead_code,
        reason = "commit sequence inspection is retained for staged audit consumers"
    )]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    #[must_use]
    #[allow(
        dead_code,
        reason = "duplicate inspection is retained for staged audit consumers"
    )]
    pub const fn was_duplicate(self) -> bool {
        self.was_duplicate
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HandoffFence {
    journal_format: u16,
    store_version: u32,
    sequence: u64,
    record_hash: RecordHash,
}

impl HandoffFence {
    const fn from_head(head: JournalHead) -> Self {
        Self {
            journal_format: FRAME_VERSION,
            store_version: STORE_VERSION,
            sequence: head.sequence,
            record_hash: head.record_hash,
        }
    }

    #[must_use]
    #[allow(
        dead_code,
        reason = "handoff sequence inspection is retained for staged audit consumers"
    )]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    pub(crate) fn authority_digest(self) -> String {
        let mut digest = CanonicalDigest::new(b"mission-journal-authority-head-v1");
        digest.u64(u64::from(self.journal_format));
        digest.u64(u64::from(self.store_version));
        digest.u64(self.sequence);
        digest.bytes(self.record_hash.as_bytes());
        digest.finish()
    }
}

#[derive(Debug)]
pub struct MissionStoreReader {
    session_data_dir: PathBuf,
    observed_fence: HandoffFence,
}

impl MissionStoreReader {
    /// Opens a non-mutating view. It never creates files or repairs a tail.
    pub fn open_existing(session_data_dir: &Path) -> Result<Self, MissionStoreError> {
        ensure_supported_platform()?;
        let directory = session_data_dir.join(STORE_DIRECTORY);
        ensure_existing_private_directory(&directory)?;
        let journal_path = directory.join(JOURNAL_FILE);
        let mut journal = FramedJournal::new(open_existing_private_file(&journal_path)?);
        let persisted = load_head(&directory.join(HEAD_FILE))?.unwrap_or(JournalHead::EMPTY);
        let snapshot_load = load_untrusted_snapshot(&directory.join(SNAPSHOT_FILE))?;
        let snapshot_sequence = snapshot_load.sequence();
        let scan = journal.scan(
            ReplayMode::Inspect,
            [Some(persisted.sequence), snapshot_sequence],
        )?;
        let snapshot = resolve_snapshot_cache(snapshot_load, &scan)?.0;
        let replay = load_journal_tail(&mut journal, &scan, snapshot)?;
        let actual = actual_head(&replay);
        validate_persisted_head(persisted, actual, &replay)?;
        if persisted != actual {
            return Err(MissionStoreError::HandoffObservationUnstable);
        }
        Ok(Self {
            session_data_dir: session_data_dir.to_path_buf(),
            observed_fence: HandoffFence::from_head(actual),
        })
    }

    #[must_use]
    pub const fn observed_fence(&self) -> HandoffFence {
        self.observed_fence
    }

    pub fn acquire_writer(self, expected: HandoffFence) -> Result<MissionStore, MissionStoreError> {
        validate_fence_format(expected)?;
        let store = MissionStore::open(&self.session_data_dir)?;
        if store.handoff_fence() != expected {
            return Err(MissionStoreError::HandoffHeadChanged);
        }
        Ok(store)
    }
}

#[derive(Debug)]
pub struct PreparedMissionStore {
    store: MissionStore,
    fence: HandoffFence,
}

impl PreparedMissionStore {
    #[must_use]
    pub const fn fence(&self) -> HandoffFence {
        self.fence
    }

    #[must_use]
    pub fn abort(self) -> MissionStore {
        self.store
    }

    #[must_use]
    pub fn relinquish(self) -> ReleasedMissionStore {
        let session_data_dir = self.store.session_data_dir.clone();
        let fence = self.fence;
        drop(self.store);
        ReleasedMissionStore {
            session_data_dir,
            fence,
        }
    }
}

#[derive(Debug)]
pub struct ReleasedMissionStore {
    session_data_dir: PathBuf,
    fence: HandoffFence,
}

impl ReleasedMissionStore {
    pub fn reacquire(self) -> Result<MissionStore, MissionStoreError> {
        let store = MissionStore::open(&self.session_data_dir)?;
        if store.handoff_fence() != self.fence {
            return Err(MissionStoreError::HandoffHeadChanged);
        }
        Ok(store)
    }
}

#[derive(Debug)]
pub struct PrepareHandoffError {
    store: Box<MissionStore>,
    source: MissionStoreError,
}

impl PrepareHandoffError {
    #[must_use]
    #[allow(dead_code)]
    pub fn into_parts(self) -> (MissionStore, MissionStoreError) {
        (*self.store, self.source)
    }
}

impl std::fmt::Display for PrepareHandoffError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "mission store handoff preparation failed: {}",
            self.source
        )
    }
}

impl std::error::Error for PrepareHandoffError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug)]
pub struct MissionStore {
    session_data_dir: PathBuf,
    directory: PathBuf,
    journal: FramedJournal,
    head_path: PathBuf,
    snapshot_path: PathBuf,
    journal_head: JournalHead,
    event_index: BTreeMap<String, EventIndexEntry>,
    #[cfg(test)]
    events: Vec<SequencedMissionEvent>,
    #[cfg(test)]
    deserialized_journal_events: u64,
    projection: MissionProjection,
    _writer_lock: File,
    poisoned: bool,
}

impl MissionStore {
    /// Opens the session-scoped mission journal and repairs only a truncated
    /// final record. Corruption anywhere else is fatal.
    pub fn open(session_data_dir: &Path) -> Result<Self, MissionStoreError> {
        ensure_supported_platform()?;
        let directory = session_data_dir.join(STORE_DIRECTORY);
        ensure_private_directory(&directory)?;
        let journal_path = directory.join(JOURNAL_FILE);
        let head_path = directory.join(HEAD_FILE);
        let snapshot_path = directory.join(SNAPSHOT_FILE);
        let journal_file = open_private_regular_file(&journal_path)?;
        let writer_lock = open_private_regular_file(&directory.join(WRITER_LOCK_FILE))?;
        if let Err(error) = writer_lock.try_lock() {
            return match error {
                std::fs::TryLockError::WouldBlock => Err(MissionStoreError::WriterAlreadyActive),
                std::fs::TryLockError::Error(error) => Err(error.into()),
            };
        }

        let mut journal = FramedJournal::new(journal_file);
        let persisted_head = load_head(&head_path)?.unwrap_or(JournalHead::EMPTY);
        let snapshot_load = load_untrusted_snapshot(&snapshot_path)?;
        let snapshot_sequence = snapshot_load.sequence();
        let scan = journal.scan(
            ReplayMode::RepairFinalPartial,
            [Some(persisted_head.sequence), snapshot_sequence],
        )?;
        let (snapshot, snapshot_corrupt) = resolve_snapshot_cache(snapshot_load, &scan)?;
        let replay = load_journal_tail(&mut journal, &scan, snapshot)?;
        let journal_head = reconcile_head(&directory, &head_path, persisted_head, &replay)?;

        let store = Self {
            session_data_dir: session_data_dir.to_path_buf(),
            directory,
            journal,
            head_path,
            snapshot_path,
            journal_head,
            event_index: replay.event_index,
            #[cfg(test)]
            events: replay.events,
            #[cfg(test)]
            deserialized_journal_events: replay.deserialized_journal_events,
            projection: replay.projection,
            _writer_lock: writer_lock,
            poisoned: false,
        };
        if snapshot_corrupt {
            store.quarantine_and_rebuild_snapshot()?;
        }
        Ok(store)
    }

    /// Checks whether an exact event is already durable without changing any
    /// projection, lease, journal, or idempotence state.
    #[allow(
        dead_code,
        reason = "preflight inspection is retained for staged external event producers"
    )]
    pub fn preflight_duplicate(
        &self,
        event_id: &str,
        event: &PersistableMissionEvent,
    ) -> Result<Option<CommitOutcome>, MissionStoreError> {
        validate_id("event id", event_id)?;
        event.validate()?;
        let event_fingerprint = event_fingerprint(event)?;
        self.duplicate_outcome(event_id, &event_fingerprint)
    }

    fn duplicate_outcome(
        &self,
        event_id: &str,
        event_fingerprint: &str,
    ) -> Result<Option<CommitOutcome>, MissionStoreError> {
        let Some(existing) = self.event_index.get(event_id) else {
            return Ok(None);
        };
        if existing.event_fingerprint.as_ref() != event_fingerprint {
            return Err(MissionStoreError::EventIdConflict(event_id.to_owned()));
        }
        Ok(Some(CommitOutcome {
            sequence: existing.sequence,
            was_duplicate: true,
        }))
    }

    /// Durably appends an allowlisted event before mutating the in-memory
    /// projection. Duplicate event IDs return their original sequence.
    pub fn commit(
        &mut self,
        event_id: &str,
        event: PersistableMissionEvent,
    ) -> Result<CommitOutcome, MissionStoreError> {
        if self.poisoned {
            return Err(MissionStoreError::WriterPoisoned);
        }
        validate_id("event id", event_id)?;
        event.validate()?;

        let event_fingerprint = event_fingerprint(&event)?;
        if let Some(outcome) = self.duplicate_outcome(event_id, &event_fingerprint)? {
            return Ok(outcome);
        }

        self.validate_current_proof_authority(&event)?;

        let mut next_projection = self.projection.clone();
        next_projection.apply(&event)?;
        let sequence = self
            .journal_head
            .sequence
            .checked_add(1)
            .ok_or(MissionStoreError::SequenceOverflow)?;
        if sequence > MAX_JOURNAL_FRAMES {
            return Err(JournalError::FrameLimitExceeded {
                limit: MAX_JOURNAL_FRAMES,
            }
            .into());
        }
        let mut next_event_index = self.event_index.clone();
        next_event_index.insert(
            event_id.to_owned(),
            EventIndexEntry {
                sequence,
                event_fingerprint: event_fingerprint.clone().into_boxed_str(),
            },
        );
        let state_digest = store_state_digest(&next_projection, &next_event_index)?;
        let record = SequencedMissionEvent {
            version: STORE_VERSION,
            sequence,
            event_id: event_id.to_owned(),
            previous_hash: self.journal_head.record_hash,
            projection_digest_after: state_digest,
            event,
        };

        let payload = serde_json::to_vec(&record)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(JournalError::PayloadTooLarge(payload.len()).into());
        }
        let record_hash = match self.journal.append(
            &payload,
            self.journal_head.record_hash,
            record.projection_digest_after,
        ) {
            Ok(hash) => hash,
            Err(error) => {
                self.poisoned = true;
                return Err(error.into());
            }
        };
        let next_head = JournalHead {
            version: STORE_VERSION,
            sequence,
            record_hash,
        };
        if let Err(error) = atomic_write_json(&self.directory, &self.head_path, &next_head) {
            self.poisoned = true;
            return Err(error);
        }

        self.journal_head = next_head;
        self.event_index = next_event_index;
        #[cfg(test)]
        self.events.push(record);
        self.projection = next_projection;
        Ok(CommitOutcome {
            sequence,
            was_duplicate: false,
        })
    }

    fn validate_current_proof_authority(
        &self,
        event: &PersistableMissionEvent,
    ) -> Result<(), MissionStoreError> {
        let receipt = match event {
            PersistableMissionEvent::MissionReady { receipt, .. }
            | PersistableMissionEvent::MissionArchived { receipt, .. } => receipt,
            _ => return Ok(()),
        };
        if receipt.authority_sequence() != self.journal_head.sequence
            || receipt.authority_head_digest() != self.handoff_fence().authority_digest()
        {
            return Err(MissionStoreError::StaleProofAuthority);
        }
        Ok(())
    }

    /// Writes a durable accelerator snapshot. The journal remains canonical
    /// and is never compacted.
    pub fn checkpoint(&self) -> Result<(), MissionStoreError> {
        let mut snapshot = MissionSnapshot {
            version: STORE_VERSION,
            last_sequence: self.journal_head.sequence,
            last_record_hash: self.journal_head.record_hash,
            journal_offset: self.journal.current_len()?,
            projection: self.projection.clone(),
            event_index: self.event_index.clone(),
            state_digest: StateHash::default(),
        };
        snapshot.state_digest = store_state_digest(&snapshot.projection, &snapshot.event_index)?;
        atomic_write_json_limited(
            &self.directory,
            &self.snapshot_path,
            &snapshot,
            MAX_SNAPSHOT_BYTES,
            FileLimitKind::Snapshot,
        )
    }

    fn quarantine_and_rebuild_snapshot(&self) -> Result<(), MissionStoreError> {
        let quarantine_path = self.directory.join(SNAPSHOT_QUARANTINE_FILE);
        verify_existing_private_file_if_present(&self.snapshot_path)?;
        verify_existing_private_file_if_present(&quarantine_path)?;
        std::fs::rename(&self.snapshot_path, &quarantine_path)?;
        sync_directory(&self.directory)?;
        self.checkpoint()
    }

    pub fn prepare_handoff(self) -> Result<PreparedMissionStore, PrepareHandoffError> {
        if let Err(source) = self.checkpoint() {
            return Err(PrepareHandoffError {
                store: Box::new(self),
                source,
            });
        }
        let fence = self.handoff_fence();
        Ok(PreparedMissionStore { store: self, fence })
    }

    #[must_use]
    pub const fn handoff_fence(&self) -> HandoffFence {
        HandoffFence::from_head(self.journal_head)
    }

    #[must_use]
    #[allow(
        dead_code,
        reason = "last sequence inspection is retained for staged audit consumers"
    )]
    pub const fn last_sequence(&self) -> u64 {
        self.journal_head.sequence
    }

    #[must_use]
    pub const fn projection(&self) -> &MissionProjection {
        &self.projection
    }

    #[cfg(test)]
    pub fn events_after(&self, sequence: u64) -> impl Iterator<Item = &SequencedMissionEvent> {
        self.events
            .iter()
            .filter(move |event| event.sequence > sequence)
    }

    #[cfg(test)]
    fn deserialized_journal_events(&self) -> u64 {
        self.deserialized_journal_events
    }
}

struct ReplayState {
    actual_head: JournalHead,
    persisted_head_hash: Option<RecordHash>,
    event_index: BTreeMap<String, EventIndexEntry>,
    projection: MissionProjection,
    #[cfg(test)]
    events: Vec<SequencedMissionEvent>,
    #[cfg(test)]
    deserialized_journal_events: u64,
}

struct VerifiedSnapshot {
    checkpoint: ReplayCheckpoint,
    projection: MissionProjection,
    event_index: BTreeMap<String, EventIndexEntry>,
}

fn load_journal_tail(
    journal: &mut FramedJournal,
    scan: &ScanSummary,
    snapshot: Option<VerifiedSnapshot>,
) -> Result<ReplayState, MissionStoreError> {
    let (checkpoint, mut projection, mut event_index) = snapshot.map_or_else(
        || {
            (
                ReplayCheckpoint {
                    sequence: 0,
                    end_offset: 0,
                    record_hash: RecordHash::ZERO,
                    state_hash: StateHash::default(),
                },
                MissionProjection::default(),
                BTreeMap::new(),
            )
        },
        |snapshot| {
            (
                snapshot.checkpoint,
                snapshot.projection,
                snapshot.event_index,
            )
        },
    );
    #[cfg(test)]
    let mut events = Vec::new();
    #[cfg(test)]
    let mut deserialized_journal_events = 0_u64;

    let replay = journal.replay_from(checkpoint, |frame| {
        let record =
            serde_json::from_slice::<SequencedMissionEvent>(&frame.payload).map_err(|error| {
                MissionStoreError::CorruptJournalPayload {
                    sequence: frame.sequence,
                    message: error.to_string(),
                }
            })?;
        #[cfg(test)]
        {
            deserialized_journal_events = deserialized_journal_events
                .checked_add(1)
                .ok_or(MissionStoreError::SequenceOverflow)?;
        }

        if record.version != STORE_VERSION {
            return Err(MissionStoreError::UnsupportedVersion(record.version));
        }
        if record.sequence != frame.sequence {
            return Err(MissionStoreError::NonContiguousSequence {
                expected: frame.sequence,
                actual: record.sequence,
            });
        }
        if record.previous_hash != frame.previous_hash {
            return Err(MissionStoreError::BrokenHashChain {
                sequence: record.sequence,
            });
        }
        if record.projection_digest_after != frame.state_hash {
            return Err(MissionStoreError::JournalProjectionMismatch {
                sequence: record.sequence,
            });
        }
        validate_id("event id", &record.event_id)?;
        record.event.validate()?;
        let event_fingerprint = event_fingerprint(&record.event)?;
        if event_index
            .insert(
                record.event_id.clone(),
                EventIndexEntry {
                    sequence: record.sequence,
                    event_fingerprint: event_fingerprint.into_boxed_str(),
                },
            )
            .is_some()
        {
            return Err(MissionStoreError::DuplicateJournalEventId(
                record.event_id.clone(),
            ));
        }
        projection.apply(&record.event)?;
        if store_state_digest(&projection, &event_index)? != frame.state_hash {
            return Err(MissionStoreError::JournalProjectionMismatch {
                sequence: record.sequence,
            });
        }
        #[cfg(test)]
        {
            events.push(record);
        }
        Ok(())
    });
    let summary = match replay {
        Ok(summary) => summary,
        Err(ReplayError::Journal(error)) => return Err(error.into()),
        Err(ReplayError::Visitor(error)) => return Err(error),
    };
    if summary.final_sequence != scan.frame_count
        || summary.bytes_consumed != scan.bytes_consumed
        || summary.last_hash != scan.last_hash
    {
        return Err(MissionStoreError::JournalChangedDuringReplay);
    }

    let actual_head = JournalHead {
        version: STORE_VERSION,
        sequence: scan.frame_count,
        record_hash: scan.last_hash,
    };

    Ok(ReplayState {
        actual_head,
        persisted_head_hash: scan.checkpoints[0].map(|checkpoint| checkpoint.record_hash),
        event_index,
        projection,
        #[cfg(test)]
        events,
        #[cfg(test)]
        deserialized_journal_events,
    })
}

fn event_fingerprint(event: &PersistableMissionEvent) -> Result<String, MissionStoreError> {
    let payload = serde_json::to_vec(event)?;
    let mut digest = CanonicalDigest::new(b"mission-event-idempotence-v1");
    digest.bytes(&payload);
    Ok(digest.finish())
}

fn load_head(path: &Path) -> Result<Option<JournalHead>, MissionStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(MissionStoreError::SymlinkNotAllowed(path.to_path_buf()));
            }
            if !metadata.is_file() {
                return Err(MissionStoreError::NotRegularFile(path.to_path_buf()));
            }
            verify_private_file_metadata(path, &metadata)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let bytes = read_limited_private_file(path, MAX_HEAD_BYTES, FileLimitKind::Head)?;
    let head: JournalHead = serde_json::from_slice(&bytes)?;
    if head.version != STORE_VERSION {
        return Err(MissionStoreError::UnsupportedVersion(head.version));
    }
    Ok(Some(head))
}

fn reconcile_head(
    directory: &Path,
    head_path: &Path,
    persisted: JournalHead,
    replay: &ReplayState,
) -> Result<JournalHead, MissionStoreError> {
    let actual = actual_head(replay);
    validate_persisted_head(persisted, actual, replay)?;
    if persisted != actual {
        atomic_write_json(directory, head_path, &actual)?;
    }
    Ok(actual)
}

fn actual_head(replay: &ReplayState) -> JournalHead {
    replay.actual_head
}

fn validate_persisted_head(
    persisted: JournalHead,
    actual: JournalHead,
    replay: &ReplayState,
) -> Result<(), MissionStoreError> {
    if persisted.sequence > actual.sequence {
        return Err(MissionStoreError::HeadAheadOfJournal {
            head: persisted.sequence,
            journal: actual.sequence,
        });
    }
    let hash_at_persisted_head = replay
        .persisted_head_hash
        .ok_or(MissionStoreError::EventIndexCorrupt)?;
    if persisted.record_hash != hash_at_persisted_head {
        return Err(MissionStoreError::HeadHashMismatch);
    }
    Ok(())
}

fn validate_fence_format(fence: HandoffFence) -> Result<(), MissionStoreError> {
    if fence.journal_format != FRAME_VERSION || fence.store_version != STORE_VERSION {
        return Err(MissionStoreError::UnsupportedHandoffFence);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize)]
struct SnapshotAnchor {
    version: u32,
    last_sequence: u64,
    last_record_hash: RecordHash,
    journal_offset: u64,
    state_digest: StateHash,
}

struct UntrustedSnapshot {
    anchor: SnapshotAnchor,
    bytes: Vec<u8>,
}

enum SnapshotLoad {
    Missing,
    Corrupt,
    Untrusted(UntrustedSnapshot),
}

impl SnapshotLoad {
    const fn sequence(&self) -> Option<u64> {
        match self {
            Self::Untrusted(snapshot) => Some(snapshot.anchor.last_sequence),
            Self::Missing | Self::Corrupt => None,
        }
    }
}

fn load_untrusted_snapshot(path: &Path) -> Result<SnapshotLoad, MissionStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(MissionStoreError::SymlinkNotAllowed(path.to_path_buf()));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(MissionStoreError::NotRegularFile(path.to_path_buf()));
        }
        Ok(metadata) => verify_private_file_metadata(path, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SnapshotLoad::Missing);
        }
        Err(error) => return Err(error.into()),
    }
    let bytes = read_limited_private_file(path, MAX_SNAPSHOT_BYTES, FileLimitKind::Snapshot)?;
    let anchor: SnapshotAnchor = match serde_json::from_slice(&bytes) {
        Ok(anchor) => anchor,
        Err(_) => return Ok(SnapshotLoad::Corrupt),
    };
    if anchor.version != STORE_VERSION {
        return Ok(SnapshotLoad::Corrupt);
    }
    Ok(SnapshotLoad::Untrusted(UntrustedSnapshot { anchor, bytes }))
}

fn resolve_snapshot_cache(
    load: SnapshotLoad,
    scan: &ScanSummary,
) -> Result<(Option<VerifiedSnapshot>, bool), MissionStoreError> {
    match load {
        SnapshotLoad::Missing => Ok((None, false)),
        SnapshotLoad::Corrupt => Ok((None, true)),
        SnapshotLoad::Untrusted(snapshot) => match authenticate_snapshot(Some(snapshot), scan) {
            Ok(snapshot) => Ok((snapshot, false)),
            Err(error) if is_snapshot_content_corruption(&error) => Ok((None, true)),
            Err(error) => Err(error),
        },
    }
}

fn is_snapshot_content_corruption(error: &MissionStoreError) -> bool {
    matches!(
        error,
        MissionStoreError::Json(_)
            | MissionStoreError::UnsupportedVersion(_)
            | MissionStoreError::InvalidIdentifier { .. }
            | MissionStoreError::InvalidHash { .. }
            | MissionStoreError::SnapshotAheadOfJournal { .. }
            | MissionStoreError::SnapshotProjectionMismatch
            | MissionStoreError::SnapshotHeadMismatch
            | MissionStoreError::SnapshotOffsetMismatch
            | MissionStoreError::SnapshotEventIndexMismatch
    )
}

fn authenticate_snapshot(
    untrusted: Option<UntrustedSnapshot>,
    scan: &ScanSummary,
) -> Result<Option<VerifiedSnapshot>, MissionStoreError> {
    let Some(untrusted) = untrusted else {
        return Ok(None);
    };
    let anchor = untrusted.anchor;
    if anchor.last_sequence > scan.frame_count {
        return Err(MissionStoreError::SnapshotAheadOfJournal {
            snapshot: anchor.last_sequence,
            journal: scan.frame_count,
        });
    }
    let checkpoint = if anchor.last_sequence == 0 {
        if anchor.last_record_hash != RecordHash::ZERO || anchor.journal_offset != 0 {
            return Err(MissionStoreError::SnapshotHeadMismatch);
        }
        ReplayCheckpoint {
            sequence: 0,
            end_offset: 0,
            record_hash: RecordHash::ZERO,
            state_hash: anchor.state_digest,
        }
    } else {
        let checkpoint = scan.checkpoints[1].ok_or(MissionStoreError::SnapshotHeadMismatch)?;
        if checkpoint.record_hash != anchor.last_record_hash {
            return Err(MissionStoreError::SnapshotHeadMismatch);
        }
        if checkpoint.end_offset != anchor.journal_offset {
            return Err(MissionStoreError::SnapshotOffsetMismatch);
        }
        if checkpoint.state_hash != anchor.state_digest {
            return Err(MissionStoreError::SnapshotProjectionMismatch);
        }
        checkpoint
    };

    // Projection and idempotence index are deserialized only after their
    // sequence, byte offset, record hash, and state digest are journal-bound.
    let snapshot: MissionSnapshot = serde_json::from_slice(&untrusted.bytes)?;
    if snapshot.version != anchor.version
        || snapshot.last_sequence != anchor.last_sequence
        || snapshot.last_record_hash != anchor.last_record_hash
        || snapshot.journal_offset != anchor.journal_offset
        || snapshot.state_digest != anchor.state_digest
    {
        return Err(MissionStoreError::SnapshotHeadMismatch);
    }
    validate_snapshot_index(&snapshot)?;
    if store_state_digest(&snapshot.projection, &snapshot.event_index)? != snapshot.state_digest {
        return Err(MissionStoreError::SnapshotProjectionMismatch);
    }
    if snapshot.last_sequence == 0
        && (snapshot.projection != MissionProjection::default() || !snapshot.event_index.is_empty())
    {
        return Err(MissionStoreError::SnapshotProjectionMismatch);
    }
    Ok(Some(VerifiedSnapshot {
        checkpoint,
        projection: snapshot.projection,
        event_index: snapshot.event_index,
    }))
}

fn validate_snapshot_index(snapshot: &MissionSnapshot) -> Result<(), MissionStoreError> {
    if snapshot.event_index.len() as u64 != snapshot.last_sequence {
        return Err(MissionStoreError::SnapshotEventIndexMismatch);
    }
    let mut sequences = BTreeSet::new();
    for (event_id, entry) in &snapshot.event_index {
        validate_id("event id", event_id)?;
        validate_hash("event fingerprint", &entry.event_fingerprint)?;
        if entry.sequence == 0
            || entry.sequence > snapshot.last_sequence
            || !sequences.insert(entry.sequence)
        {
            return Err(MissionStoreError::SnapshotEventIndexMismatch);
        }
    }
    Ok(())
}

fn store_state_digest(
    projection: &MissionProjection,
    event_index: &BTreeMap<String, EventIndexEntry>,
) -> Result<StateHash, MissionStoreError> {
    let mut writer = ProjectionDigestWriter::new();
    let serialized = serde_json::to_writer(&mut writer, &(projection, event_index));
    if writer.limit_exceeded {
        return Err(MissionStoreError::SnapshotTooLarge {
            bytes: MAX_SNAPSHOT_BYTES.saturating_add(1),
            limit: MAX_SNAPSHOT_BYTES,
        });
    }
    serialized?;
    Ok(writer.finish())
}

struct ProjectionDigestWriter {
    hasher: Sha256,
    bytes: u64,
    limit_exceeded: bool,
}

impl ProjectionDigestWriter {
    fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"mission-projection-state-v1\0");
        Self {
            hasher,
            bytes: 0,
            limit_exceeded: false,
        }
    }

    fn finish(self) -> StateHash {
        StateHash::from_bytes(self.hasher.finalize().into())
    }
}

impl Write for ProjectionDigestWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let next = self.bytes.saturating_add(buffer.len() as u64);
        if next > MAX_SNAPSHOT_BYTES {
            self.limit_exceeded = true;
            return Err(std::io::Error::other(
                "mission projection exceeds snapshot limit",
            ));
        }
        self.hasher.update(buffer);
        self.bytes = next;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum FileLimitKind {
    Head,
    Snapshot,
}

impl FileLimitKind {
    fn error(self, bytes: u64, limit: u64) -> MissionStoreError {
        match self {
            Self::Head => MissionStoreError::HeadTooLarge { bytes, limit },
            Self::Snapshot => MissionStoreError::SnapshotTooLarge { bytes, limit },
        }
    }
}

fn read_limited_private_file(
    path: &Path,
    limit: u64,
    kind: FileLimitKind,
) -> Result<Vec<u8>, MissionStoreError> {
    let file = open_existing_private_file(path)?;
    let declared_length = file.metadata()?.len();
    if declared_length > limit {
        return Err(kind.error(declared_length, limit));
    }
    let capacity =
        usize::try_from(declared_length).map_err(|_| kind.error(declared_length, limit))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    let actual_length = bytes.len() as u64;
    if actual_length > limit {
        return Err(kind.error(actual_length, limit));
    }
    Ok(bytes)
}

fn atomic_write_json<T: Serialize>(
    directory: &Path,
    destination: &Path,
    value: &T,
) -> Result<(), MissionStoreError> {
    atomic_write_json_limited(
        directory,
        destination,
        value,
        MAX_HEAD_BYTES,
        FileLimitKind::Head,
    )
}

fn atomic_write_json_limited<T: Serialize>(
    directory: &Path,
    destination: &Path,
    value: &T,
    limit: u64,
    kind: FileLimitKind,
) -> Result<(), MissionStoreError> {
    #[cfg(test)]
    if FAIL_NEXT_ATOMIC_WRITE_WITH_STORAGE_FULL
        .lock()
        .expect("storage-full test fault lock should not be poisoned")
        .as_ref()
        .is_some_and(|target| target == destination)
    {
        *FAIL_NEXT_ATOMIC_WRITE_WITH_STORAGE_FULL
            .lock()
            .expect("storage-full test fault lock should not be poisoned") = None;
        return Err(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "simulated storage exhaustion",
        )
        .into());
    }

    verify_existing_private_file_if_present(destination)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let destination_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| MissionStoreError::InvalidStorePath(destination.to_path_buf()))?;
    let temporary = directory.join(format!(
        ".{destination_name}.{}.{}.tmp",
        std::process::id(),
        nonce
    ));
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    let byte_count = bytes.len() as u64;
    if byte_count > limit {
        return Err(kind.error(byte_count, limit));
    }
    let mut file = private_open_options()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, destination)?;
    sync_directory(directory)?;
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), MissionStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(MissionStoreError::SymlinkNotAllowed(path.to_path_buf()));
            }
            if !metadata.is_dir() {
                return Err(MissionStoreError::InvalidStorePath(path.to_path_buf()));
            }
            verify_private_directory_metadata(path, &metadata)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_directory(path)?;
            verify_private_directory_metadata(path, &std::fs::symlink_metadata(path)?)?;
            sync_directory(path)?;
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn ensure_existing_private_directory(path: &Path) -> Result<(), MissionStoreError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(MissionStoreError::SymlinkNotAllowed(path.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Err(MissionStoreError::InvalidStorePath(path.to_path_buf()));
    }
    verify_private_directory_metadata(path, &metadata)?;
    Ok(())
}

fn open_private_regular_file(path: &Path) -> Result<File, MissionStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(MissionStoreError::SymlinkNotAllowed(path.to_path_buf()));
            }
            if !metadata.is_file() {
                return Err(MissionStoreError::NotRegularFile(path.to_path_buf()));
            }
            verify_private_file_metadata(path, &metadata)?;
            let file = private_open_options().read(true).append(true).open(path)?;
            let opened_metadata = file.metadata()?;
            if !opened_metadata.is_file() {
                return Err(MissionStoreError::NotRegularFile(path.to_path_buf()));
            }
            verify_private_file_metadata(path, &opened_metadata)?;
            Ok(file)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let file = private_open_options()
                .read(true)
                .append(true)
                .create_new(true)
                .open(path)?;
            set_private_open_file_permissions(&file)?;
            verify_private_file_metadata(path, &file.metadata()?)?;
            file.sync_all()?;
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            Ok(file)
        }
        Err(error) => Err(error.into()),
    }
}

fn verify_existing_private_file_if_present(path: &Path) -> Result<(), MissionStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(MissionStoreError::SymlinkNotAllowed(path.to_path_buf()))
        }
        Ok(metadata) if !metadata.is_file() => {
            Err(MissionStoreError::NotRegularFile(path.to_path_buf()))
        }
        Ok(metadata) => verify_private_file_metadata(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn open_existing_private_file(path: &Path) -> Result<File, MissionStoreError> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(MissionStoreError::NotRegularFile(path.to_path_buf()));
    }
    verify_private_file_metadata(path, &metadata)?;
    Ok(file)
}

#[cfg(unix)]
fn verify_private_directory_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), MissionStoreError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(MissionStoreError::WrongOwner(path.to_path_buf()));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(MissionStoreError::InsecurePermissions(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_private_directory_metadata(
    path: &Path,
    _metadata: &std::fs::Metadata,
) -> Result<(), MissionStoreError> {
    Err(MissionStoreError::UnsupportedPlatform(path.to_path_buf()))
}

#[cfg(unix)]
fn verify_private_file_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), MissionStoreError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(MissionStoreError::WrongOwner(path.to_path_buf()));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(MissionStoreError::InsecurePermissions(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_private_file_metadata(
    path: &Path,
    _metadata: &std::fs::Metadata,
) -> Result<(), MissionStoreError> {
    Err(MissionStoreError::UnsupportedPlatform(path.to_path_buf()))
}

fn reject_symlink(path: &Path) -> Result<(), MissionStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(MissionStoreError::SymlinkNotAllowed(path.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn private_open_options() -> OpenOptions {
    let options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut options = options;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        options
    }
    #[cfg(not(unix))]
    {
        options
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<(), MissionStoreError> {
    use std::os::unix::fs::DirBuilderExt as _;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700).create(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> Result<(), MissionStoreError> {
    std::fs::create_dir(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_open_file_permissions(file: &File) -> Result<(), MissionStoreError> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_open_file_permissions(_file: &File) -> Result<(), MissionStoreError> {
    Err(MissionStoreError::UnsupportedPlatform(PathBuf::new()))
}

#[cfg(unix)]
const fn ensure_supported_platform() -> Result<(), MissionStoreError> {
    Ok(())
}

#[cfg(not(unix))]
fn ensure_supported_platform() -> Result<(), MissionStoreError> {
    Err(MissionStoreError::UnsupportedPlatform(PathBuf::new()))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), MissionStoreError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), MissionStoreError> {
    Ok(())
}

pub(crate) fn validate_mission_id(value: &str) -> Result<(), MissionStoreError> {
    validate_id("mission id", value)
}

fn validate_id(label: &'static str, value: &str) -> Result<(), MissionStoreError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(MissionStoreError::InvalidIdentifier { label });
    }
    Ok(())
}

fn validate_hash(label: &'static str, value: &str) -> Result<(), MissionStoreError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(MissionStoreError::InvalidHash { label });
    }
    Ok(())
}

fn validate_revision(value: &str) -> Result<(), MissionStoreError> {
    if !(40..=64).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(MissionStoreError::InvalidRevision);
    }
    Ok(())
}

fn repository_path_hash(repository_path: &str) -> String {
    let mut digest = CanonicalDigest::new(b"mission-repository-path-v1");
    digest.string(repository_path);
    digest.finish()
}

fn validate_text(
    label: &'static str,
    value: &str,
    minimum_bytes: usize,
    maximum_bytes: usize,
) -> Result<(), MissionStoreError> {
    if value.trim().len() < minimum_bytes
        || value.len() > maximum_bytes
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(MissionStoreError::InvalidText { label });
    }
    Ok(())
}

fn validate_proof_receipt(receipt: &ProofReceipt) -> Result<(), MissionStoreError> {
    validate_hash("proof subject", receipt.subject_digest())?;
    validate_hash("proof seal", receipt.seal_digest())?;
    validate_hash("proof authority head", receipt.authority_head_digest())?;
    validate_hash("proof worktree lease", receipt.lease_digest())?;
    if receipt.authority_sequence() == 0 {
        return Err(MissionStoreError::InvalidProofReceipt);
    }
    Ok(())
}

fn validate_opaque_id(label: &'static str, value: &str) -> Result<(), MissionStoreError> {
    if value.is_empty()
        || value.len() > 256
        || value.chars().any(char::is_control)
        || !value.is_ascii()
    {
        return Err(MissionStoreError::InvalidIdentifier { label });
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<(), MissionStoreError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err(MissionStoreError::InvalidRelativePath);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum MissionStoreError {
    #[error("mission store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("mission store JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("mission journal framing failed: {0}")]
    Journal(#[from] JournalError),
    #[error("mission store already has an active writer")]
    WriterAlreadyActive,
    #[error("mission store writer is poisoned after an ambiguous I/O result")]
    WriterPoisoned,
    #[error("mission store path is not a regular file: {0}")]
    NotRegularFile(PathBuf),
    #[error("mission store path is invalid: {0}")]
    InvalidStorePath(PathBuf),
    #[error("invalid {label}")]
    InvalidIdentifier { label: &'static str },
    #[error("invalid {label}")]
    InvalidHash { label: &'static str },
    #[error("invalid {label}")]
    InvalidText { label: &'static str },
    #[error("mission repository path must be absolute")]
    RepositoryPathNotAbsolute,
    #[error("mission worktree path must be absolute")]
    WorktreePathNotAbsolute,
    #[error("mission base revision is invalid")]
    InvalidRevision,
    #[error("mission repository hash does not match its path")]
    RepositoryHashMismatch,
    #[error("mission acceptance criteria are invalid")]
    InvalidAcceptanceCriteria,
    #[error("worktree path must be relative and cannot escape its repository")]
    InvalidRelativePath,
    #[error("mission store version {0} is not supported")]
    UnsupportedVersion(u32),
    #[error("mission journal payload {sequence} is corrupt: {message}")]
    CorruptJournalPayload { sequence: u64, message: String },
    #[error("mission journal hash chain is broken at sequence {sequence}")]
    BrokenHashChain { sequence: u64 },
    #[error("mission journal projection digest is invalid at sequence {sequence}")]
    JournalProjectionMismatch { sequence: u64 },
    #[error("mission journal expected sequence {expected}, got {actual}")]
    NonContiguousSequence { expected: u64, actual: u64 },
    #[error("mission journal contains duplicate event id {0}")]
    DuplicateJournalEventId(String),
    #[error("mission event id {0} was reused with a different payload")]
    EventIdConflict(String),
    #[error("mission event index is inconsistent with its journal")]
    EventIndexCorrupt,
    #[error("mission sequence overflow")]
    SequenceOverflow,
    #[error("mission {0} already exists")]
    MissionAlreadyExists(String),
    #[error("mission {0} does not exist")]
    MissionNotFound(String),
    #[error("mission run already started")]
    RunAlreadyStarted,
    #[error("mission run id was already used by this mission")]
    RunAlreadyExists,
    #[error("mission closure plan is invalid")]
    InvalidClosurePlan,
    #[error("mission closure plan is missing")]
    ClosureMissing,
    #[error("mission closure plan is already configured")]
    ClosureAlreadyConfigured,
    #[error("mission closure plan cannot be changed after a run starts")]
    ClosureCannotBeChanged,
    #[error("mission run is missing")]
    RunMissing,
    #[error("mission run does not match the durable run")]
    RunMismatch,
    #[error("handoff target must differ from the source provider")]
    HandoffSameProvider,
    #[error("handoff requires all source-run attention to be resolved")]
    HandoffAttentionUnresolved,
    #[error("attention {0} does not exist")]
    AttentionNotFound(String),
    #[error("response attempt generation and attempt must be greater than zero")]
    InvalidResponseAttempt,
    #[error("response attempt already exists")]
    ResponseAttemptAlreadyExists,
    #[error("response attempt does not exist")]
    ResponseAttemptNotFound,
    #[error("response attempt state transition is invalid")]
    InvalidResponseState,
    #[error("response route does not match the durable provider run")]
    ResponseRouteMismatch,
    #[error("attention state must be changed by its dedicated durable event")]
    SealedAttentionState,
    #[error("invalid persisted mission transition: {} -> {}", from.as_str(), to.as_str())]
    InvalidStatusTransition {
        from: MissionStatus,
        to: MissionStatus,
    },
    #[error("ready_to_close and archived require sealed mission events")]
    SealedStatusRequiresProof,
    #[error("sealed proof belongs to another mission")]
    #[allow(
        dead_code,
        reason = "sealed proof events are staged until public mission closure"
    )]
    ProofMissionMismatch,
    #[error("sealed proof receipt is invalid")]
    InvalidProofReceipt,
    #[error("sealed proof authority is stale")]
    StaleProofAuthority,
    #[error("sealed proof timestamp is in the future")]
    ProofFromFuture,
    #[error("ready-to-close projection has no proof receipt")]
    ReadyReceiptMissing,
    #[error("archive proof does not bind the durable ready receipt")]
    ProofBasisChanged,
    #[error("archive requires a new durable authority revision")]
    FreshArchiveProofRequired,
    #[error("mission snapshot sequence {snapshot} is ahead of journal sequence {journal}")]
    SnapshotAheadOfJournal { snapshot: u64, journal: u64 },
    #[error("mission snapshot projection does not match its journal")]
    SnapshotProjectionMismatch,
    #[error("mission snapshot journal head does not match the journal")]
    SnapshotHeadMismatch,
    #[error("mission snapshot journal offset does not match the journal")]
    SnapshotOffsetMismatch,
    #[error("mission snapshot event index is inconsistent")]
    SnapshotEventIndexMismatch,
    #[error("mission journal changed while replaying its authenticated tail")]
    JournalChangedDuringReplay,
    #[error("mission snapshot is too large: {bytes} bytes exceeds the {limit} byte limit")]
    SnapshotTooLarge { bytes: u64, limit: u64 },
    #[error("mission journal head is too large: {bytes} bytes exceeds the {limit} byte limit")]
    HeadTooLarge { bytes: u64, limit: u64 },
    #[error("mission journal head sequence {head} is ahead of journal sequence {journal}")]
    HeadAheadOfJournal { head: u64, journal: u64 },
    #[error("mission journal head hash does not match its recorded sequence")]
    HeadHashMismatch,
    #[error("mission handoff reader observed journal data not committed by its head")]
    HandoffObservationUnstable,
    #[error("mission journal changed across the handoff ownership fence")]
    HandoffHeadChanged,
    #[error("mission handoff fence format is not supported")]
    UnsupportedHandoffFence,
    #[error("mission store path cannot be a symlink: {0}")]
    SymlinkNotAllowed(PathBuf),
    #[error("mission store path is not owned by the current user: {0}")]
    WrongOwner(PathBuf),
    #[error("mission store path grants access to group or other users: {0}")]
    InsecurePermissions(PathBuf),
    #[error(
        "mission store cannot enforce private ownership and permissions on this platform: {0}"
    )]
    #[allow(
        dead_code,
        reason = "this platform guard is unreachable on Unix builds"
    )]
    UnsupportedPlatform(PathBuf),
}

#[cfg(test)]
mod bounded_replay_tests {
    use crate::mission::{
        attention::{
            AttentionEvent, AttentionInbox, AttentionKind, ProviderResponseIntent,
            ResponseCapability,
        },
        evidence::{CommandSpec, PathRule},
        run_state::ObservationSource,
    };

    use super::*;

    fn created(mission_id: &str, title: &str) -> PersistableMissionEvent {
        PersistableMissionEvent::mission_created(
            mission_id,
            title,
            "/repo",
            "Keep replay bounded",
            vec!["The journal remains valid".to_owned()],
            10,
        )
        .unwrap()
    }

    #[test]
    fn storage_full_after_journal_append_poisoned_writer_recovers_exactly_on_restart() {
        let directory = tempfile::tempdir().unwrap();
        let event = created("mission-disk-full", "Recover disk pressure");
        let mut store = MissionStore::open(directory.path()).unwrap();

        fail_next_atomic_write_with_storage_full(&store.head_path);
        let error = store.commit("event-disk-full", event.clone()).unwrap_err();
        assert!(matches!(
            error,
            MissionStoreError::Io(ref source)
                if source.kind() == std::io::ErrorKind::StorageFull
        ));
        assert!(store
            .projection()
            .mission_view("mission-disk-full")
            .is_none());
        assert!(matches!(
            store.commit("event-after-disk-full", created("mission-after", "Blocked")),
            Err(MissionStoreError::WriterPoisoned)
        ));
        drop(store);

        let mut recovered = MissionStore::open(directory.path()).unwrap();
        assert_eq!(recovered.last_sequence(), 1);
        assert!(recovered
            .projection()
            .mission_view("mission-disk-full")
            .is_some());
        let duplicate = recovered.commit("event-disk-full", event).unwrap();
        assert!(duplicate.was_duplicate());
        assert_eq!(duplicate.sequence(), 1);
    }

    #[test]
    fn a_verified_snapshot_accelerates_projection_then_replays_its_tail() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        store
            .commit("event-1", created("mission-1", "Before"))
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
        assert_eq!(
            restored.projection().mission_status("mission-1"),
            Some(MissionStatus::Preparing)
        );
        assert_eq!(restored.deserialized_journal_events(), 1);
    }

    #[test]
    fn a_falsified_snapshot_is_quarantined_and_rebuilt_from_the_journal() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        store
            .commit("event-1", created("mission-1", "Original"))
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

        let path = directory.path().join("missions/missions.snapshot.json");
        let mut snapshot: MissionSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        snapshot
            .projection
            .missions
            .get_mut("mission-1")
            .unwrap()
            .title = "Falsified".to_owned();
        snapshot.state_digest =
            store_state_digest(&snapshot.projection, &snapshot.event_index).unwrap();
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let restored = MissionStore::open(directory.path()).unwrap();
        assert_eq!(
            restored
                .projection()
                .mission_view("mission-1")
                .unwrap()
                .title,
            "Original"
        );
        assert!(directory
            .path()
            .join("missions/missions.snapshot.invalid.json")
            .exists());
        let rebuilt: MissionSnapshot = serde_json::from_slice(
            &std::fs::read(directory.path().join("missions/missions.snapshot.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(rebuilt.last_sequence, 2);
    }

    #[test]
    fn a_future_snapshot_anchor_is_ignored_and_rebuilt() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        store
            .commit("event-1", created("mission-1", "Original"))
            .unwrap();
        store.checkpoint().unwrap();
        drop(store);

        let path = directory.path().join("missions/missions.snapshot.json");
        let mut snapshot: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        snapshot["last_sequence"] = serde_json::json!(2);
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let restored = MissionStore::open(directory.path()).unwrap();
        assert_eq!(restored.last_sequence(), 1);
    }

    #[test]
    fn a_stale_snapshot_sequence_with_a_newer_hash_is_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        store
            .commit("event-1", created("mission-1", "First"))
            .unwrap();
        store
            .commit("event-2", created("mission-2", "Second"))
            .unwrap();
        store.checkpoint().unwrap();
        drop(store);

        let path = directory.path().join("missions/missions.snapshot.json");
        let mut snapshot: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        snapshot["last_sequence"] = serde_json::json!(1);
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        let restored = MissionStore::open(directory.path()).unwrap();
        assert_eq!(restored.last_sequence(), 2);
        assert!(restored.projection().mission_view("mission-1").is_some());
        assert!(restored.projection().mission_view("mission-2").is_some());
    }

    #[test]
    fn a_snapshot_with_a_bad_offset_or_state_anchor_is_ignored() {
        for corrupt_state in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut store = MissionStore::open(directory.path()).unwrap();
            store
                .commit("event-1", created("mission-1", "Original"))
                .unwrap();
            store.checkpoint().unwrap();
            drop(store);

            let path = directory.path().join("missions/missions.snapshot.json");
            let mut snapshot: MissionSnapshot =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            if corrupt_state {
                snapshot.state_digest = StateHash::from_bytes([0x7f; 32]);
            } else {
                snapshot.journal_offset += 1;
            }
            std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

            let restored = MissionStore::open(directory.path()).unwrap();
            assert_eq!(restored.last_sequence(), 1);
        }
    }

    #[test]
    fn malformed_snapshot_json_is_quarantined_and_rebuilt_by_the_writer() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        store
            .commit("event-1", created("mission-1", "Canonical"))
            .unwrap();
        store.checkpoint().unwrap();
        drop(store);

        let snapshot = directory.path().join("missions/missions.snapshot.json");
        let corrupt = br#"{"projection":"truncated""#;
        std::fs::write(&snapshot, corrupt).unwrap();

        let restored = MissionStore::open(directory.path()).unwrap();
        assert_eq!(
            restored
                .projection()
                .mission_view("mission-1")
                .unwrap()
                .title,
            "Canonical"
        );
        assert_eq!(
            std::fs::read(
                directory
                    .path()
                    .join("missions/missions.snapshot.invalid.json")
            )
            .unwrap(),
            corrupt
        );
        assert!(
            serde_json::from_slice::<MissionSnapshot>(&std::fs::read(snapshot).unwrap()).is_ok()
        );
    }

    #[test]
    fn handoff_reader_ignores_a_corrupt_snapshot_without_mutating_files() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        store
            .commit("event-1", created("mission-1", "Canonical"))
            .unwrap();
        store.checkpoint().unwrap();
        drop(store);

        let snapshot = directory.path().join("missions/missions.snapshot.json");
        let corrupt = br#"{"projection":"truncated""#;
        std::fs::write(&snapshot, corrupt).unwrap();

        let reader = MissionStoreReader::open_existing(directory.path()).unwrap();
        assert_eq!(reader.observed_fence().sequence(), 1);
        assert_eq!(std::fs::read(&snapshot).unwrap(), corrupt);
        assert!(!directory
            .path()
            .join("missions/missions.snapshot.invalid.json")
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn prepare_handoff_error_is_small_and_returns_the_live_store() {
        use std::os::unix::fs::PermissionsExt as _;

        assert!(std::mem::size_of::<PrepareHandoffError>() <= 128);
        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        store
            .commit("event-1", created("mission-1", "Handoff"))
            .unwrap();
        store.checkpoint().unwrap();
        let snapshot = directory.path().join("missions/missions.snapshot.json");
        std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o666)).unwrap();

        let error = store.prepare_handoff().unwrap_err();
        let (mut recovered, source) = error.into_parts();
        assert!(matches!(source, MissionStoreError::InsecurePermissions(path) if path == snapshot));
        std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o600)).unwrap();
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
        assert_eq!(recovered.last_sequence(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_symlink_remains_a_fatal_security_error() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        store
            .commit("event-1", created("mission-1", "Canonical"))
            .unwrap();
        store.checkpoint().unwrap();
        drop(store);

        let snapshot = directory.path().join("missions/missions.snapshot.json");
        let target = directory.path().join("attacker-controlled.json");
        std::fs::rename(&snapshot, &target).unwrap();
        symlink(&target, &snapshot).unwrap();

        assert!(matches!(
            MissionStore::open(directory.path()),
            Err(MissionStoreError::SymlinkNotAllowed(path)) if path == snapshot
        ));
        assert!(!directory
            .path()
            .join("missions/missions.snapshot.invalid.json")
            .exists());
    }

    #[test]
    fn idempotence_remains_exact_on_both_sides_of_a_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let before = created("mission-1", "Before");
        let after = created("mission-2", "After");
        let mut store = MissionStore::open(directory.path()).unwrap();
        store.commit("event-before", before.clone()).unwrap();
        store.checkpoint().unwrap();
        store.commit("event-after", after.clone()).unwrap();
        drop(store);

        let mut restored = MissionStore::open(directory.path()).unwrap();
        let before_duplicate = restored.commit("event-before", before).unwrap();
        let after_duplicate = restored.commit("event-after", after).unwrap();
        assert!(before_duplicate.was_duplicate());
        assert_eq!(before_duplicate.sequence(), 1);
        assert!(after_duplicate.was_duplicate());
        assert_eq!(after_duplicate.sequence(), 2);
        assert!(matches!(
            restored.commit("event-before", created("mission-3", "Conflict")),
            Err(MissionStoreError::EventIdConflict(_))
        ));
    }

    #[test]
    fn corruption_before_a_snapshot_is_still_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        store
            .commit("event-1", created("mission-1", "Before"))
            .unwrap();
        store.checkpoint().unwrap();
        store
            .commit("event-2", created("mission-2", "After"))
            .unwrap();
        drop(store);

        let journal_path = directory.path().join("missions/missions.journal.bin");
        let mut bytes = std::fs::read(&journal_path).unwrap();
        bytes[crate::mission::journal::FRAME_HEADER_LEN + 4] ^= 1;
        std::fs::write(journal_path, bytes).unwrap();

        assert!(matches!(
            MissionStore::open(directory.path()),
            Err(MissionStoreError::Journal(
                JournalError::RecordHashMismatch { .. }
            ))
        ));
    }

    #[test]
    fn an_oversized_snapshot_fails_without_being_read_into_memory() {
        let directory = tempfile::tempdir().unwrap();
        let store = MissionStore::open(directory.path()).unwrap();
        store.checkpoint().unwrap();
        drop(store);

        let snapshot_path = directory.path().join("missions/missions.snapshot.json");
        OpenOptions::new()
            .write(true)
            .open(snapshot_path)
            .unwrap()
            .set_len(16 * 1024 * 1024 + 1)
            .unwrap();

        assert!(matches!(
            MissionStore::open(directory.path()),
            Err(MissionStoreError::SnapshotTooLarge { .. })
        ));
    }

    #[test]
    fn clock_rollback_uses_logical_time_but_preserves_the_observed_event_time() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        let mut initial = created("mission-1", "Clock");
        if let PersistableMissionEvent::MissionCreated { at_millis, .. } = &mut initial {
            *at_millis = 100;
        }
        store.commit("event-1", initial).unwrap();
        store
            .commit(
                "event-2",
                PersistableMissionEvent::StatusChanged {
                    mission_id: "mission-1".to_owned(),
                    status: MissionStatus::Preparing,
                    at_millis: 50,
                },
            )
            .unwrap();

        let mission = store.projection().mission_view("mission-1").unwrap();
        assert_eq!(mission.updated_at_millis, 100);
        let observed_time = store
            .events_after(1)
            .find_map(|record| match &record.event {
                PersistableMissionEvent::StatusChanged { at_millis, .. } => Some(*at_millis),
                _ => None,
            });
        assert_eq!(observed_time, Some(50));
    }

    fn seed_managed_attention(store: &mut MissionStore) {
        store
            .commit("event-1", created("mission-1", "Route"))
            .unwrap();
        let criterion_ids =
            MissionDefinition::criterion_ids(&["The journal remains valid".to_owned()]);
        store
            .commit(
                "event-2",
                PersistableMissionEvent::closure_configured(
                    "mission-1",
                    vec![CheckDeclaration::command(
                        "tests",
                        CommandSpec::new("cargo", ["test"], "."),
                        vec![PathRule::All],
                        Vec::new(),
                    )
                    .covers(criterion_ids)],
                    20,
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
                    30,
                )
                .unwrap(),
            )
            .unwrap();
        store
            .commit(
                "event-4",
                PersistableMissionEvent::provider_session_bound(
                    "mission-1",
                    "run-1",
                    "thread/abc",
                    40,
                )
                .unwrap(),
            )
            .unwrap();
        store
            .commit(
                "event-5",
                PersistableMissionEvent::AttentionChanged {
                    mission_id: "mission-1".to_owned(),
                    attention_id: "attention-1".to_owned(),
                    state: PersistedAttentionState::Open,
                    risk: AttentionRisk::High,
                    at_millis: 50,
                },
            )
            .unwrap();
    }

    #[test]
    fn response_route_mismatch_matrix_is_rejected_without_mutating_attention() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        seed_managed_attention(&mut store);
        let key = ResponseAttemptKey::new("attention-1", 1, 1).unwrap();
        let routes = [
            PersistedResponseRoute::managed(
                ProviderKind::ClaudeCode,
                "run-1",
                "thread/abc",
                "request-1",
            ),
            PersistedResponseRoute::managed(
                ProviderKind::Codex,
                "different-run",
                "thread/abc",
                "request-1",
            ),
            PersistedResponseRoute::new(
                ProviderKind::Codex,
                "run-1",
                "thread/abc",
                PaneTarget::new("workspace-1", "pane-1"),
                "request-1",
            ),
            PersistedResponseRoute::managed(
                ProviderKind::Codex,
                "run-1",
                "different-session",
                "request-1",
            ),
        ];

        for (index, route) in routes.into_iter().enumerate() {
            let result = store.commit(
                &format!("event-mismatch-{index}"),
                PersistableMissionEvent::ResponseRequested {
                    mission_id: "mission-1".to_owned(),
                    key: key.clone(),
                    route,
                    decision: AttentionDecision::ApproveOnce,
                    actor_id: "local-user".to_owned(),
                    at_millis: 60,
                },
            );

            assert!(matches!(
                result,
                Err(MissionStoreError::ResponseRouteMismatch)
            ));
            assert_eq!(store.last_sequence(), 5);
            assert_eq!(
                store
                    .projection()
                    .attention_state("mission-1", "attention-1"),
                Some(PersistedAttentionState::Open)
            );
            assert_eq!(store.projection().response_state("mission-1", &key), None);
        }
    }

    #[test]
    fn slash_provider_session_ids_are_valid_and_raw_answers_never_persist() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        seed_managed_attention(&mut store);
        let raw_provider_answer = "MUXORA-RAW-ANSWER-SENTINEL-93f2";
        let inbox = AttentionInbox::new()
            .ingest(
                AttentionEvent::new(
                    "attention-1",
                    "mission-1",
                    "run-1",
                    "thread/abc",
                    PaneTarget::new("workspace-1", "pane-1"),
                    AttentionKind::ProviderQuestion,
                    "Which deployment target should I use?",
                    "worktree-a",
                    AttentionRisk::High,
                    ProviderKind::Codex,
                    ObservationSource::ProviderApi,
                    50,
                )
                .with_provider_request_id("request-1")
                .with_response_capability(ResponseCapability::Reliable),
            )
            .unwrap();
        let (pending, response) =
            inbox.answer("attention-1", raw_provider_answer, "local-user", 60);
        let Some(ProviderResponseIntent::Respond {
            route,
            decision,
            answer: Some(answer),
            token,
        }) = response.unwrap()
        else {
            panic!("expected the normal provider-answer response intent");
        };
        assert_eq!(answer.expose_to_provider(), raw_provider_answer);
        assert_eq!(
            pending
                .item("attention-1")
                .unwrap()
                .response_attempts()
                .len(),
            1
        );

        let token_json = serde_json::to_value(&token).unwrap();
        assert_eq!(token_json["item_id"], "attention-1");
        assert_eq!(token_json["request_generation"], 1);
        assert_eq!(token_json["attempt"], 1);
        let key = ResponseAttemptKey::new(
            token_json["item_id"].as_str().unwrap(),
            token_json["request_generation"].as_u64().unwrap(),
            u32::try_from(token_json["attempt"].as_u64().unwrap()).unwrap(),
        )
        .unwrap();
        let requested = PersistableMissionEvent::ResponseRequested {
            mission_id: route.mission_id().to_owned(),
            key: key.clone(),
            route: PersistedResponseRoute::managed(
                route.provider(),
                route.mission_run_id(),
                route.session_id(),
                route.provider_request_id(),
            ),
            decision,
            actor_id: "local-user".to_owned(),
            at_millis: 60,
        };
        let requested_fingerprint = event_fingerprint(&requested).unwrap();
        let attempt_digest = key.digest();

        store.commit("event-6", requested.clone()).unwrap();
        store.checkpoint().unwrap();
        assert_eq!(
            store.projection().response_state("mission-1", &key),
            Some(&PersistedResponseState::Requested)
        );
        assert_eq!(
            store
                .projection()
                .attention_state("mission-1", "attention-1"),
            Some(PersistedAttentionState::PendingResponse)
        );
        let duplicate = store
            .preflight_duplicate("event-6", &requested)
            .unwrap()
            .unwrap();
        assert!(duplicate.was_duplicate());
        assert_eq!(duplicate.sequence(), 6);

        for file in ["missions.journal.bin", "missions.snapshot.json"] {
            let bytes = std::fs::read(directory.path().join("missions").join(file)).unwrap();
            assert!(!bytes
                .windows(raw_provider_answer.len())
                .any(|window| window == raw_provider_answer.as_bytes()));
        }
        let snapshot =
            std::fs::read_to_string(directory.path().join("missions/missions.snapshot.json"))
                .unwrap();
        assert!(snapshot.contains(&attempt_digest));
        assert!(snapshot.contains(&requested_fingerprint));
    }

    #[cfg(unix)]
    #[test]
    fn existing_store_files_with_group_or_world_access_are_rejected_without_chmod() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let mut store = MissionStore::open(directory.path()).unwrap();
        store
            .commit("event-1", created("mission-1", "Permissions"))
            .unwrap();
        store.checkpoint().unwrap();
        drop(store);

        for file in [JOURNAL_FILE, WRITER_LOCK_FILE, HEAD_FILE, SNAPSHOT_FILE] {
            let path = directory.path().join(STORE_DIRECTORY).join(file);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

            assert!(matches!(
                MissionStore::open(directory.path()),
                Err(MissionStoreError::InsecurePermissions(ref rejected)) if rejected == &path
            ));
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o666
            );

            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn unsupported_platforms_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            MissionStore::open(directory.path()),
            Err(MissionStoreError::UnsupportedPlatform(_))
        ));
        assert!(matches!(
            MissionStoreReader::open_existing(directory.path()),
            Err(MissionStoreError::UnsupportedPlatform(_))
        ));
    }

    #[test]
    fn duplicate_preflight_is_exact_and_never_mutates_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let event = created("mission-1", "Preflight");
        let mut store = MissionStore::open(directory.path()).unwrap();
        store.commit("event-1", event.clone()).unwrap();
        store.checkpoint().unwrap();
        drop(store);

        let store = MissionStore::open(directory.path()).unwrap();
        let duplicate = store
            .preflight_duplicate("event-1", &event)
            .unwrap()
            .unwrap();
        assert!(duplicate.was_duplicate());
        assert_eq!(duplicate.sequence(), 1);
        assert_eq!(
            store.preflight_duplicate("new-event", &event).unwrap(),
            None
        );
        assert!(matches!(
            store.preflight_duplicate("event-1", &created("mission-2", "Conflict")),
            Err(MissionStoreError::EventIdConflict(_))
        ));
        assert_eq!(store.last_sequence(), 1);
    }
}
