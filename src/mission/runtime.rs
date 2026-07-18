use std::path::Path;

#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use super::{
    claims::{
        ClaimRequestId, LeaseOwner, ReleaseOutcome, WorktreeClaimError, WorktreeClaimRegistry,
        WorktreeLease,
    },
    digest::CanonicalDigest,
    evidence::CheckDeclaration,
    model::{MissionDefinition, MissionStatus, ProviderKind, ProviderMode},
    proof::ClosurePlan,
    store::{
        CommitOutcome, MissionStore, MissionStoreError, MissionStoreReader, MissionView,
        PersistableMissionEvent, PreparedMissionStore, ReleasedMissionStore,
    },
};

#[cfg(unix)]
use super::store::HandoffFence;

#[cfg(test)]
use super::{
    evidence::{EvidenceRecord, WorkspaceSnapshot},
    proof::{digest_attention, digest_evidence, ProofIdentity},
};

#[derive(Debug)]
pub(crate) struct AuthoritySnapshot {
    identity_digest: String,
    workspace_digest: String,
    attention_digest: String,
    evidence_digest: String,
    head_digest: String,
    lease_digest: String,
    sequence: u64,
    captured_at_millis: u64,
}

impl AuthoritySnapshot {
    pub(super) fn identity_digest(&self) -> &str {
        &self.identity_digest
    }

    pub(super) fn workspace_digest(&self) -> &str {
        &self.workspace_digest
    }

    pub(super) fn attention_digest(&self) -> &str {
        &self.attention_digest
    }

    pub(super) fn evidence_digest(&self) -> &str {
        &self.evidence_digest
    }

    pub(super) fn head_digest(&self) -> &str {
        &self.head_digest
    }

    pub(super) fn lease_digest(&self) -> &str {
        &self.lease_digest
    }

    pub(super) const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(super) const fn captured_at_millis(&self) -> u64 {
        self.captured_at_millis
    }

    #[cfg(test)]
    pub(super) fn for_test(
        identity: &ProofIdentity,
        current: &WorkspaceSnapshot,
        records: &BTreeMap<String, EvidenceRecord>,
        unresolved_attention_ids: &BTreeSet<String>,
        sequence: u64,
        captured_at_millis: u64,
    ) -> Self {
        let mut head = super::digest::CanonicalDigest::new(b"mission-test-authority-head-v1");
        head.u64(sequence);
        let mut lease = super::digest::CanonicalDigest::new(b"mission-test-lease-v1");
        lease.u64(sequence);
        Self {
            identity_digest: identity.digest(),
            workspace_digest: current.digest(),
            attention_digest: digest_attention(unresolved_attention_ids),
            evidence_digest: digest_evidence(records),
            head_digest: head.finish(),
            lease_digest: lease.finish(),
            sequence,
            captured_at_millis,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test_at_head(
        identity: &ProofIdentity,
        current: &WorkspaceSnapshot,
        records: &BTreeMap<String, EvidenceRecord>,
        unresolved_attention_ids: &BTreeSet<String>,
        sequence: u64,
        captured_at_millis: u64,
        head_digest: impl Into<String>,
    ) -> Self {
        let mut snapshot = Self::for_test(
            identity,
            current,
            records,
            unresolved_attention_ids,
            sequence,
            captured_at_millis,
        );
        snapshot.head_digest = head_digest.into();
        snapshot
    }
}

#[derive(Debug)]
enum Ownership {
    Owned(MissionStore),
    Prepared(PreparedMissionStore),
    Released(ReleasedMissionStore),
    Observing(MissionStoreReader),
    #[allow(
        dead_code,
        reason = "the disabled runtime is retained for unsupported platform boundaries"
    )]
    Disabled,
    Vacant,
}

/// Server-owned authority for the durable mission journal.
///
/// The headless server is the only production owner. Live handoff moves this
/// state through prepare, release, and acquire without ever creating two
/// writers for the same session journal.
#[derive(Debug)]
pub(crate) struct MissionRuntime {
    ownership: Ownership,
    claims: Option<WorktreeClaimRegistry>,
}

#[derive(Debug)]
pub(crate) struct CreateMission {
    pub(crate) mission_id: String,
    pub(crate) title: String,
    pub(crate) repository_path: String,
    pub(crate) objective: String,
    pub(crate) acceptance_criteria: Vec<String>,
    pub(crate) at_millis: u64,
}

#[derive(Debug)]
pub(crate) struct CreateMissionOutcome {
    pub(crate) mission: MissionView,
    pub(crate) created: bool,
}

#[derive(Debug)]
pub(crate) struct ConfigureMission {
    pub(crate) mission_id: String,
    pub(crate) declarations: Vec<CheckDeclaration>,
    pub(crate) at_millis: u64,
}

#[derive(Debug)]
pub(crate) struct ConfigureMissionOutcome {
    pub(crate) mission: MissionView,
    pub(crate) configured: bool,
}

#[derive(Debug)]
pub(crate) struct StartRun {
    pub(crate) mission_id: String,
    pub(crate) run_id: String,
    pub(crate) provider: ProviderKind,
    pub(crate) mode: ProviderMode,
    pub(crate) worktree_path: String,
    pub(crate) request_id: ClaimRequestId,
    pub(crate) at_millis: u64,
}

#[derive(Debug)]
pub(crate) struct StartRunOutcome {
    pub(crate) mission: MissionView,
    pub(crate) lease: WorktreeLease,
}

impl MissionRuntime {
    pub(crate) fn open_owned(
        session_data_dir: &Path,
        global_claim_directory: &Path,
    ) -> Result<Self, MissionRuntimeError> {
        Ok(Self {
            ownership: Ownership::Owned(MissionStore::open(session_data_dir)?),
            claims: Some(WorktreeClaimRegistry::new(global_claim_directory)?),
        })
    }

    #[allow(
        dead_code,
        reason = "the disabled runtime is retained for unsupported platform boundaries"
    )]
    pub(crate) const fn disabled() -> Self {
        Self {
            ownership: Ownership::Disabled,
            claims: None,
        }
    }

    pub(crate) const fn is_available(&self) -> bool {
        !matches!(self.ownership, Ownership::Disabled)
    }

    fn ensure_available(&self) -> Result<(), MissionRuntimeError> {
        if self.is_available() {
            Ok(())
        } else {
            Err(MissionRuntimeError::FeatureUnavailable)
        }
    }

    fn claims(&self) -> Result<&WorktreeClaimRegistry, MissionRuntimeError> {
        self.claims
            .as_ref()
            .ok_or(MissionRuntimeError::FeatureUnavailable)
    }

    #[cfg(unix)]
    pub(crate) fn observe_handoff(
        session_data_dir: &Path,
        global_claim_directory: &Path,
        expected: HandoffFence,
    ) -> Result<Self, MissionRuntimeError> {
        let reader = MissionStoreReader::open_existing(session_data_dir)?;
        if reader.observed_fence() != expected {
            return Err(MissionRuntimeError::FenceMismatch);
        }
        Ok(Self {
            ownership: Ownership::Observing(reader),
            claims: Some(WorktreeClaimRegistry::new(global_claim_directory)?),
        })
    }

    #[cfg(test)]
    pub(crate) fn capture_authority(
        &self,
        lease: &WorktreeLease,
        identity: &ProofIdentity,
        current: &WorkspaceSnapshot,
        records: &BTreeMap<String, EvidenceRecord>,
        unresolved_attention_ids: &BTreeSet<String>,
        captured_at_millis: u64,
    ) -> Result<AuthoritySnapshot, MissionRuntimeError> {
        let Ownership::Owned(store) = &self.ownership else {
            return Err(MissionRuntimeError::InvalidOwnership);
        };
        if !self.claims()?.is_current(lease)? {
            return Err(MissionRuntimeError::LeaseNotCurrent);
        }
        let checkout_root = std::fs::canonicalize(identity.worktree_identity())
            .map_err(|_| MissionRuntimeError::LeaseScopeMismatch)?;
        if !lease.matches_scope(identity.mission_id(), identity.run_id(), &checkout_root) {
            return Err(MissionRuntimeError::LeaseScopeMismatch);
        }
        let sequence = store.last_sequence();
        if sequence == 0 {
            return Err(MissionRuntimeError::EmptyAuthority);
        }
        Ok(AuthoritySnapshot {
            identity_digest: identity.digest(),
            workspace_digest: current.digest(),
            attention_digest: digest_attention(unresolved_attention_ids),
            evidence_digest: digest_evidence(records),
            head_digest: store.handoff_fence().authority_digest(),
            lease_digest: lease.authority_digest(),
            sequence,
            captured_at_millis,
        })
    }

    pub(crate) fn commit(
        &mut self,
        event_id: &str,
        event: PersistableMissionEvent,
    ) -> Result<CommitOutcome, MissionRuntimeError> {
        self.ensure_available()?;
        if let Some(lease_digest) = event.proof_lease_digest() {
            if !self.claims()?.has_current_authority_digest(lease_digest)? {
                return Err(MissionRuntimeError::LeaseNotCurrent);
            }
        }
        let Ownership::Owned(store) = &mut self.ownership else {
            return Err(MissionRuntimeError::InvalidOwnership);
        };
        Ok(store.commit(event_id, event)?)
    }

    pub(crate) fn create_mission(
        &mut self,
        mut request: CreateMission,
    ) -> Result<CreateMissionOutcome, MissionRuntimeError> {
        let repository = std::fs::canonicalize(&request.repository_path)
            .map_err(|_| MissionRuntimeError::RepositoryUnavailable)?;
        let repository_info = crate::workspace::git_worktree_info(&repository)
            .ok_or(MissionRuntimeError::RepositoryNotGit)?;
        let repository_root = std::fs::canonicalize(repository_info.repo_root)
            .map_err(|_| MissionRuntimeError::RepositoryUnavailable)?;
        if repository != repository_root {
            return Err(MissionRuntimeError::RepositoryMustBeRoot);
        }
        request.repository_path = repository.to_string_lossy().into_owned();
        if let Some(existing) = self.mission(&request.mission_id) {
            if existing.title == request.title
                && existing.repository_path == request.repository_path
                && existing.objective == request.objective
                && existing.acceptance_criteria == request.acceptance_criteria
            {
                return Ok(CreateMissionOutcome {
                    mission: existing,
                    created: false,
                });
            }
            return Err(MissionRuntimeError::MissionConflict);
        }
        let event = PersistableMissionEvent::mission_created(
            &request.mission_id,
            request.title,
            request.repository_path,
            request.objective,
            request.acceptance_criteria,
            request.at_millis,
        )?;
        let mut event_id = CanonicalDigest::new(b"mission-create-event-id-v1");
        event_id.string(&request.mission_id);
        self.commit(&format!("mission-create:{}", event_id.finish()), event)?;
        Ok(CreateMissionOutcome {
            mission: self
                .mission(&request.mission_id)
                .ok_or(MissionRuntimeError::MissionMissing)?,
            created: true,
        })
    }

    pub(crate) fn mission(&self, mission_id: &str) -> Option<MissionView> {
        let Ownership::Owned(store) = &self.ownership else {
            return None;
        };
        store.projection().mission_view(mission_id)
    }

    pub(crate) fn configure_mission(
        &mut self,
        request: ConfigureMission,
    ) -> Result<ConfigureMissionOutcome, MissionRuntimeError> {
        let mission = self
            .mission(&request.mission_id)
            .ok_or(MissionRuntimeError::MissionMissing)?;
        let requested_digest = declaration_set_digest(&request.declarations);
        if !mission.check_declarations.is_empty() {
            if declaration_set_digest(&mission.check_declarations) == requested_digest {
                return Ok(ConfigureMissionOutcome {
                    mission,
                    configured: false,
                });
            }
            return Err(MissionRuntimeError::ClosureConflict);
        }
        let definition = MissionDefinition::new(
            &mission.mission_id,
            &mission.title,
            &mission.repository_path,
            &mission.objective,
            mission.acceptance_criteria.clone(),
        )
        .map_err(|_| MissionRuntimeError::InvalidClosurePlan)?;
        ClosurePlan::new(
            &definition.acceptance_criterion_ids(),
            &request.declarations,
        )
        .map_err(|_| MissionRuntimeError::InvalidClosurePlan)?;
        let event = PersistableMissionEvent::closure_configured(
            &request.mission_id,
            request.declarations,
            request.at_millis,
        )?;
        let mut event_id = CanonicalDigest::new(b"mission-closure-config-event-id-v1");
        event_id.string(&request.mission_id);
        event_id.string(&requested_digest);
        self.commit(&format!("closure-config:{}", event_id.finish()), event)?;
        Ok(ConfigureMissionOutcome {
            mission: self
                .mission(&request.mission_id)
                .ok_or(MissionRuntimeError::MissionMissing)?,
            configured: true,
        })
    }

    pub(crate) fn start_run(
        &mut self,
        request: StartRun,
    ) -> Result<StartRunOutcome, MissionRuntimeError> {
        let mission = self
            .mission(&request.mission_id)
            .ok_or(MissionRuntimeError::MissionMissing)?;
        if mission.status != MissionStatus::Draft || mission.run.is_some() {
            return Err(MissionRuntimeError::RunAlreadyStarted);
        }
        if mission.check_declarations.is_empty() {
            return Err(MissionRuntimeError::ClosureMissing);
        }
        let repository = std::fs::canonicalize(&mission.repository_path)
            .map_err(|_| MissionRuntimeError::RepositoryUnavailable)?;
        let worktree = std::fs::canonicalize(&request.worktree_path)
            .map_err(|_| MissionRuntimeError::RepositoryUnavailable)?;
        let lease = self.claim_worktree(
            LeaseOwner::new(&request.mission_id, &request.run_id)?,
            &repository,
            &worktree,
            request.request_id,
        )?;
        let base_revision = match git_head_revision(&worktree) {
            Ok(revision) => revision,
            Err(error) => {
                let _ = self.release_worktree(&lease);
                return Err(error);
            }
        };
        let event = match PersistableMissionEvent::run_started(
            &request.mission_id,
            &request.run_id,
            request.provider,
            request.mode,
            worktree.to_string_lossy(),
            base_revision,
            request.at_millis,
        ) {
            Ok(event) => event,
            Err(error) => {
                let _ = self.release_worktree(&lease);
                return Err(error.into());
            }
        };
        let mut event_id = CanonicalDigest::new(b"mission-run-start-event-id-v1");
        event_id.string(&request.mission_id);
        event_id.string(&request.run_id);
        if let Err(error) = self.commit(&format!("run-start:{}", event_id.finish()), event) {
            let _ = self.release_worktree(&lease);
            return Err(error);
        }
        Ok(StartRunOutcome {
            mission: self
                .mission(&request.mission_id)
                .ok_or(MissionRuntimeError::MissionMissing)?,
            lease,
        })
    }

    pub(crate) fn recover_managed_run(
        &self,
        mission_id: &str,
        request_id: ClaimRequestId,
    ) -> Result<StartRunOutcome, MissionRuntimeError> {
        let mission = self
            .mission(mission_id)
            .ok_or(MissionRuntimeError::MissionMissing)?;
        let run = mission
            .run
            .as_ref()
            .ok_or(MissionRuntimeError::RecoveryNotSafe)?;
        if mission.status != MissionStatus::Active
            || mission.unresolved_attention_count != 0
            || run.mode != ProviderMode::Managed
            || run.provider_session_id.is_none()
        {
            return Err(MissionRuntimeError::RecoveryNotSafe);
        }
        let repository = std::fs::canonicalize(&mission.repository_path)
            .map_err(|_| MissionRuntimeError::RepositoryUnavailable)?;
        let worktree = std::fs::canonicalize(&run.worktree_path)
            .map_err(|_| MissionRuntimeError::RepositoryUnavailable)?;
        let lease = self.claim_worktree(
            LeaseOwner::new(mission_id, &run.run_id)?,
            &repository,
            &worktree,
            request_id,
        )?;
        Ok(StartRunOutcome { mission, lease })
    }

    pub(crate) fn bind_provider_session(
        &mut self,
        mission_id: &str,
        run_id: &str,
        provider_session_id: &str,
        at_millis: u64,
    ) -> Result<MissionView, MissionRuntimeError> {
        let event = PersistableMissionEvent::provider_session_bound(
            mission_id,
            run_id,
            provider_session_id,
            at_millis,
        )?;
        let mut event_id = CanonicalDigest::new(b"mission-provider-session-event-id-v1");
        event_id.string(mission_id);
        event_id.string(run_id);
        event_id.string(provider_session_id);
        self.commit(&format!("provider-session:{}", event_id.finish()), event)?;
        self.mission(mission_id)
            .ok_or(MissionRuntimeError::MissionMissing)
    }

    pub(crate) fn transition_run(
        &mut self,
        mission_id: &str,
        status: MissionStatus,
        at_millis: u64,
    ) -> Result<MissionView, MissionRuntimeError> {
        let mut event_id = CanonicalDigest::new(b"mission-run-status-event-id-v1");
        event_id.string(mission_id);
        event_id.string(status.as_str());
        event_id.u64(at_millis);
        self.commit(
            &format!("run-status:{}", event_id.finish()),
            PersistableMissionEvent::StatusChanged {
                mission_id: mission_id.to_owned(),
                status,
                at_millis,
            },
        )?;
        self.mission(mission_id)
            .ok_or(MissionRuntimeError::MissionMissing)
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
    ) -> Result<u32, MissionRuntimeError> {
        let Ownership::Owned(store) = &self.ownership else {
            return Err(MissionRuntimeError::InvalidOwnership);
        };
        Ok(store.projection().next_response_attempt(
            mission_id,
            attention_id,
            request_generation,
        )?)
    }

    pub(crate) fn missions(&self) -> Vec<MissionView> {
        let Ownership::Owned(store) = &self.ownership else {
            return Vec::new();
        };
        store.projection().mission_views()
    }

    pub(crate) fn claim_worktree(
        &self,
        owner: LeaseOwner,
        mission_repository: &Path,
        requested_checkout: &Path,
        request_id: ClaimRequestId,
    ) -> Result<WorktreeLease, MissionRuntimeError> {
        Ok(self
            .claims()?
            .claim(owner, mission_repository, requested_checkout, request_id)?)
    }

    pub(crate) fn release_worktree(
        &self,
        lease: &WorktreeLease,
    ) -> Result<ReleaseOutcome, MissionRuntimeError> {
        Ok(self.claims()?.release(lease)?)
    }

    #[cfg(unix)]
    pub(crate) fn prepare_handoff(&mut self) -> Result<HandoffFence, MissionRuntimeError> {
        let ownership = std::mem::replace(&mut self.ownership, Ownership::Vacant);
        match ownership {
            Ownership::Owned(store) => match store.prepare_handoff() {
                Ok(prepared) => {
                    let fence = prepared.fence();
                    self.ownership = Ownership::Prepared(prepared);
                    Ok(fence)
                }
                Err(error) => {
                    let (store, source) = error.into_parts();
                    self.ownership = Ownership::Owned(store);
                    Err(source.into())
                }
            },
            Ownership::Prepared(prepared) => {
                let fence = prepared.fence();
                self.ownership = Ownership::Prepared(prepared);
                Ok(fence)
            }
            other => {
                self.ownership = other;
                Err(MissionRuntimeError::InvalidOwnership)
            }
        }
    }

    #[cfg(unix)]
    pub(crate) fn relinquish_handoff(&mut self) -> Result<(), MissionRuntimeError> {
        let ownership = std::mem::replace(&mut self.ownership, Ownership::Vacant);
        match ownership {
            Ownership::Prepared(prepared) => {
                self.ownership = Ownership::Released(prepared.relinquish());
                Ok(())
            }
            other => {
                self.ownership = other;
                Err(MissionRuntimeError::InvalidOwnership)
            }
        }
    }

    #[cfg(unix)]
    pub(crate) fn acquire_handoff(
        &mut self,
        expected: HandoffFence,
    ) -> Result<(), MissionRuntimeError> {
        let ownership = std::mem::replace(&mut self.ownership, Ownership::Vacant);
        match ownership {
            Ownership::Observing(reader) => {
                let store = reader.acquire_writer(expected)?;
                self.ownership = Ownership::Owned(store);
                Ok(())
            }
            other => {
                self.ownership = other;
                Err(MissionRuntimeError::InvalidOwnership)
            }
        }
    }

    #[cfg(unix)]
    pub(crate) fn abort_handoff(&mut self) -> Result<(), MissionRuntimeError> {
        let ownership = std::mem::replace(&mut self.ownership, Ownership::Vacant);
        match ownership {
            Ownership::Owned(store) => {
                self.ownership = Ownership::Owned(store);
                Ok(())
            }
            Ownership::Prepared(prepared) => {
                self.ownership = Ownership::Owned(prepared.abort());
                Ok(())
            }
            Ownership::Released(released) => {
                let store = released.reacquire()?;
                self.ownership = Ownership::Owned(store);
                Ok(())
            }
            other => {
                self.ownership = other;
                Err(MissionRuntimeError::InvalidOwnership)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn is_owned(&self) -> bool {
        matches!(self.ownership, Ownership::Owned(_))
    }
}

#[derive(Debug, Error)]
pub(crate) enum MissionRuntimeError {
    #[error("mission features are unavailable on this platform")]
    FeatureUnavailable,
    #[error(transparent)]
    Store(#[from] MissionStoreError),
    #[error(transparent)]
    WorktreeClaim(#[from] WorktreeClaimError),
    #[error("mission handoff fence does not match the observed journal")]
    FenceMismatch,
    #[error("mission runtime is not in the required ownership state")]
    InvalidOwnership,
    #[error("mission authority requires a current worktree lease")]
    LeaseNotCurrent,
    #[error("mission authority worktree lease does not match the proof scope")]
    #[allow(
        dead_code,
        reason = "proof scope validation is staged until public mission closure"
    )]
    LeaseScopeMismatch,
    #[error("mission authority journal has no durable event")]
    #[allow(
        dead_code,
        reason = "proof authority validation is staged until public mission closure"
    )]
    EmptyAuthority,
    #[error("mission projection is missing after a durable commit")]
    MissionMissing,
    #[error("mission id already exists with a different specification")]
    MissionConflict,
    #[error("mission closure plan already exists with a different specification")]
    ClosureConflict,
    #[error("mission closure plan is invalid")]
    InvalidClosurePlan,
    #[error("mission closure plan must be configured before a run starts")]
    ClosureMissing,
    #[error("mission repository is unavailable")]
    RepositoryUnavailable,
    #[error("mission repository is not a Git checkout")]
    RepositoryNotGit,
    #[error("mission repository path must be its checkout root")]
    RepositoryMustBeRoot,
    #[error("mission already has a durable run")]
    RunAlreadyStarted,
    #[error("mission worktree HEAD revision is unavailable")]
    RevisionUnavailable,
    #[error("managed mission run cannot be recovered without an active session and zero unresolved attention items")]
    RecoveryNotSafe,
}

fn declaration_set_digest(declarations: &[CheckDeclaration]) -> String {
    let mut declarations = declarations
        .iter()
        .map(CheckDeclaration::digest)
        .collect::<Vec<_>>();
    declarations.sort_unstable();
    let mut digest = CanonicalDigest::new(b"mission-check-declaration-set-v1");
    digest.u64(declarations.len() as u64);
    for declaration in declarations {
        digest.string(&declaration);
    }
    digest.finish()
}

fn git_head_revision(worktree: &Path) -> Result<String, MissionRuntimeError> {
    super::verifier::TrustedGit::discover()
        .and_then(|git| git.head_revision(worktree))
        .map_err(|_| MissionRuntimeError::RevisionUnavailable)
}
