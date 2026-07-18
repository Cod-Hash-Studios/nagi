use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    digest::CanonicalDigest,
    evidence::{CheckDeclaration, EvidenceRecord, WorkspaceSnapshot},
    proof::{ClosurePlan, ProofEvaluator, ProofIdentity, ProofReport, VerifiedProof},
    runtime::AuthoritySnapshot,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MissionDefinition {
    id: String,
    title: String,
    repository_path: String,
    objective: String,
    acceptance_criteria: Vec<String>,
}

impl MissionDefinition {
    pub fn new<I, S>(
        id: impl Into<String>,
        title: impl Into<String>,
        repository_path: impl Into<String>,
        objective: impl Into<String>,
        acceptance_criteria: I,
    ) -> Result<Self, MissionError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let id = id.into();
        let title = title.into();
        let repository_path = repository_path.into();
        let objective = objective.into();
        let acceptance_criteria = acceptance_criteria
            .into_iter()
            .map(Into::into)
            .filter(|criterion: &String| !criterion.trim().is_empty())
            .collect::<Vec<_>>();

        if id.trim().is_empty() {
            return Err(MissionError::EmptyMissionId);
        }
        if title.trim().is_empty() {
            return Err(MissionError::EmptyTitle);
        }
        if !Path::new(&repository_path).is_absolute() {
            return Err(MissionError::RepositoryPathNotAbsolute);
        }
        if objective.trim().is_empty() {
            return Err(MissionError::EmptyObjective);
        }
        if acceptance_criteria.is_empty() {
            return Err(MissionError::MissingAcceptanceCriteria);
        }

        Ok(Self {
            id,
            title,
            repository_path,
            objective,
            acceptance_criteria,
        })
    }

    #[must_use]
    #[allow(
        dead_code,
        reason = "the standalone mission lifecycle is staged behind the durable runtime"
    )]
    pub fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn acceptance_criterion_ids(&self) -> Vec<String> {
        Self::criterion_ids(&self.acceptance_criteria)
    }

    pub(crate) fn criterion_ids(acceptance_criteria: &[String]) -> Vec<String> {
        acceptance_criteria
            .iter()
            .enumerate()
            .map(|(index, criterion)| {
                let mut digest = CanonicalDigest::new(b"mission-criterion-v1");
                digest.u64(index as u64);
                digest.string(criterion);
                digest.finish()
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Codex,
    ClaudeCode,
    OpenCode,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderMode {
    Managed,
    Passthrough,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[allow(
    dead_code,
    reason = "the standalone mission lifecycle is staged behind the durable runtime"
)]
pub struct RunTarget {
    run_id: String,
    provider: ProviderKind,
    mode: ProviderMode,
    base_revision: String,
    worktree_path: String,
}

#[allow(
    dead_code,
    reason = "the standalone mission lifecycle is staged behind the durable runtime"
)]
impl RunTarget {
    pub fn new(
        run_id: impl Into<String>,
        provider: ProviderKind,
        mode: ProviderMode,
        base_revision: impl Into<String>,
        worktree_path: impl Into<String>,
    ) -> Result<Self, MissionError> {
        let run_id = run_id.into();
        let base_revision = base_revision.into();
        let worktree_path = worktree_path.into();

        if run_id.trim().is_empty() {
            return Err(MissionError::EmptyRunId);
        }
        if base_revision.trim().is_empty() {
            return Err(MissionError::EmptyBaseRevision);
        }
        if !Path::new(&worktree_path).is_absolute() {
            return Err(MissionError::WorktreePathNotAbsolute);
        }

        Ok(Self {
            run_id,
            provider,
            mode,
            base_revision,
            worktree_path,
        })
    }

    #[must_use]
    pub const fn mode(&self) -> ProviderMode {
        self.mode
    }

    #[must_use]
    pub fn worktree_path(&self) -> &str {
        &self.worktree_path
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    #[must_use]
    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionStatus {
    Draft,
    Preparing,
    Active,
    ReviewRequired,
    ReadyToClose,
    Blocked,
    Failed,
    Archived,
}

impl MissionStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Preparing => "preparing",
            Self::Active => "active",
            Self::ReviewRequired => "review_required",
            Self::ReadyToClose => "ready_to_close",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Archived => "archived",
        }
    }

    pub(crate) const fn can_persist_from(self, from: Self) -> bool {
        matches!(
            (from, self),
            (Self::Draft, Self::Preparing)
                | (Self::Preparing, Self::Active)
                | (Self::Preparing, Self::Blocked)
                | (Self::Preparing, Self::Failed)
                | (Self::Active, Self::ReviewRequired)
                | (Self::Active, Self::Blocked)
                | (Self::Active, Self::Failed)
                | (Self::Blocked, Self::Preparing)
                | (Self::Blocked, Self::Active)
                | (Self::Blocked, Self::Failed)
                | (Self::ReviewRequired, Self::Active)
                | (Self::ReviewRequired, Self::Blocked)
                | (Self::ReviewRequired, Self::Failed)
                | (Self::ReadyToClose, Self::Active)
                | (Self::ReadyToClose, Self::Blocked)
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[allow(
    dead_code,
    reason = "lifecycle transition history is staged behind public mission closure"
)]
pub struct MissionTransition {
    from: Option<MissionStatus>,
    to: MissionStatus,
    actor: String,
    reason: String,
    at_millis: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[allow(
    dead_code,
    reason = "the standalone mission lifecycle is staged behind the durable runtime"
)]
pub struct MissionLifecycle {
    definition: MissionDefinition,
    status: MissionStatus,
    run_target: Option<RunTarget>,
    closure_plan: Option<ClosurePlan>,
    history: Vec<MissionTransition>,
    ready_receipt: Option<ProofReceipt>,
    archive_receipt: Option<ProofReceipt>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ProofReceipt {
    subject_digest: String,
    seal_digest: String,
    authority_head_digest: String,
    lease_digest: String,
    authority_sequence: u64,
    verified_at_millis: u64,
}

#[derive(Debug)]
#[allow(
    dead_code,
    reason = "sealed ready transitions are staged behind public mission closure"
)]
pub struct ReadyProof {
    mission_id: String,
    verified: VerifiedProof,
}

#[derive(Debug)]
#[allow(
    dead_code,
    reason = "sealed archive transitions are staged behind public mission closure"
)]
pub struct ArchiveProof {
    mission_id: String,
    verified: VerifiedProof,
    ready_seal_digest: String,
}

impl ProofReceipt {
    #[allow(
        dead_code,
        reason = "proof receipts are minted only by the staged closure transition"
    )]
    fn from_verified(proof: &VerifiedProof) -> Self {
        Self {
            subject_digest: proof.subject_digest(),
            seal_digest: proof.seal_digest(),
            authority_head_digest: proof.authority_head_digest().to_owned(),
            lease_digest: proof.lease_digest().to_owned(),
            authority_sequence: proof.authority_sequence(),
            verified_at_millis: proof.verified_at_millis(),
        }
    }

    pub(crate) fn subject_digest(&self) -> &str {
        &self.subject_digest
    }

    pub(crate) fn seal_digest(&self) -> &str {
        &self.seal_digest
    }

    pub(crate) fn authority_head_digest(&self) -> &str {
        &self.authority_head_digest
    }

    pub(crate) fn lease_digest(&self) -> &str {
        &self.lease_digest
    }

    pub(crate) const fn authority_sequence(&self) -> u64 {
        self.authority_sequence
    }

    pub(crate) const fn verified_at_millis(&self) -> u64 {
        self.verified_at_millis
    }
}

#[allow(
    dead_code,
    reason = "sealed ready transitions are staged behind public mission closure"
)]
impl ReadyProof {
    pub(crate) fn into_receipt(self) -> (String, ProofReceipt) {
        let receipt = ProofReceipt::from_verified(&self.verified);
        (self.mission_id, receipt)
    }
}

#[allow(
    dead_code,
    reason = "sealed archive transitions are staged behind public mission closure"
)]
impl ArchiveProof {
    pub(crate) fn into_receipt(self) -> (String, String, ProofReceipt) {
        let receipt = ProofReceipt::from_verified(&self.verified);
        (self.mission_id, self.ready_seal_digest, receipt)
    }
}

#[allow(
    dead_code,
    reason = "the standalone mission lifecycle is staged behind the durable runtime"
)]
impl MissionLifecycle {
    #[must_use]
    pub fn draft(definition: MissionDefinition, at_millis: u64) -> Self {
        Self {
            definition,
            status: MissionStatus::Draft,
            run_target: None,
            closure_plan: None,
            history: vec![MissionTransition {
                from: None,
                to: MissionStatus::Draft,
                actor: "system".to_owned(),
                reason: "mission created".to_owned(),
                at_millis,
            }],
            ready_receipt: None,
            archive_receipt: None,
        }
    }

    pub fn with_run_target(
        mut self,
        target: RunTarget,
        closure_checks: &[CheckDeclaration],
        actor: impl Into<String>,
        at_millis: u64,
    ) -> Result<Self, MissionError> {
        if !matches!(self.status, MissionStatus::Draft | MissionStatus::Failed) {
            return Err(MissionError::RunTargetCannotBeChanged(self.status));
        }

        let closure_plan =
            ClosurePlan::new(&self.definition.acceptance_criterion_ids(), closure_checks)
                .map_err(|_| MissionError::InvalidClosurePlan)?;
        self.run_target = Some(target);
        self.closure_plan = Some(closure_plan);
        self.record_transition(
            MissionStatus::Preparing,
            actor.into(),
            "run target configured".to_owned(),
            at_millis,
        )?;
        Ok(self)
    }

    pub fn transition(
        mut self,
        next: MissionStatus,
        actor: impl Into<String>,
        reason: impl Into<String>,
        at_millis: u64,
    ) -> Result<Self, MissionError> {
        if matches!(next, MissionStatus::ReadyToClose | MissionStatus::Archived) {
            return Err(MissionError::SealedTransitionRequired(next));
        }
        if !Self::can_transition(self.status, next) {
            return Err(MissionError::InvalidTransition {
                from: self.status,
                to: next,
            });
        }
        if self.status == MissionStatus::ReadyToClose {
            self.ready_receipt = None;
        }
        self.record_transition(next, actor.into(), reason.into(), at_millis)?;
        Ok(self)
    }

    pub fn mark_ready_to_close(
        mut self,
        proof: ReadyProof,
        actor: impl Into<String>,
        reason: impl Into<String>,
        at_millis: u64,
    ) -> Result<Self, MissionError> {
        if self.status != MissionStatus::ReviewRequired {
            return Err(MissionError::InvalidTransition {
                from: self.status,
                to: MissionStatus::ReadyToClose,
            });
        }
        let proof = proof.verified;
        let identity = self.proof_identity()?;
        if !proof.matches_identity(&identity) {
            return Err(MissionError::ProofScopeMismatch);
        }
        let closure_plan = self
            .closure_plan
            .as_ref()
            .ok_or(MissionError::ClosurePlanMissing)?;
        if !proof.matches_closure_plan(closure_plan) {
            return Err(MissionError::ProofContractMismatch);
        }
        if proof.verified_at_millis() > at_millis {
            return Err(MissionError::ProofFromFuture);
        }

        self.ready_receipt = Some(ProofReceipt::from_verified(&proof));
        self.record_transition(
            MissionStatus::ReadyToClose,
            actor.into(),
            reason.into(),
            at_millis,
        )?;
        Ok(self)
    }

    pub fn archive_with_fresh_proof(
        mut self,
        proof: ArchiveProof,
        actor: impl Into<String>,
        reason: impl Into<String>,
        at_millis: u64,
    ) -> Result<Self, MissionError> {
        if self.status != MissionStatus::ReadyToClose {
            return Err(MissionError::InvalidTransition {
                from: self.status,
                to: MissionStatus::Archived,
            });
        }
        let ready = self
            .ready_receipt
            .as_ref()
            .ok_or(MissionError::ReadyReceiptMissing)?;
        if proof.ready_seal_digest != ready.seal_digest {
            return Err(MissionError::ProofBasisChanged);
        }
        let proof = proof.verified;
        let identity = self.proof_identity()?;
        if !proof.matches_identity(&identity) {
            return Err(MissionError::ProofScopeMismatch);
        }
        let closure_plan = self
            .closure_plan
            .as_ref()
            .ok_or(MissionError::ClosurePlanMissing)?;
        if !proof.matches_closure_plan(closure_plan) {
            return Err(MissionError::ProofContractMismatch);
        }
        if proof.subject_digest() != ready.subject_digest {
            return Err(MissionError::ProofBasisChanged);
        }
        if proof.authority_sequence() <= ready.authority_sequence
            || proof.verified_at_millis() < ready.verified_at_millis
            || proof.verified_at_millis() > at_millis
        {
            return Err(MissionError::FreshArchiveProofRequired);
        }

        self.archive_receipt = Some(ProofReceipt::from_verified(&proof));
        self.record_transition(
            MissionStatus::Archived,
            actor.into(),
            reason.into(),
            at_millis,
        )?;
        Ok(self)
    }

    pub(crate) fn proof_identity(&self) -> Result<ProofIdentity, MissionError> {
        let target = self
            .run_target
            .as_ref()
            .ok_or(MissionError::MissionHasNoRunTarget)?;
        ProofIdentity::new(
            self.definition.id(),
            target.run_id(),
            &self.definition.repository_path,
            target.worktree_path(),
            target.base_revision(),
        )
        .map_err(|_| MissionError::InvalidProofIdentity)
    }

    pub(crate) fn evaluate_proof(
        &self,
        declarations: &[CheckDeclaration],
        records: &BTreeMap<String, EvidenceRecord>,
        current: &WorkspaceSnapshot,
        unresolved_attention_ids: &BTreeSet<String>,
        authority: &AuthoritySnapshot,
    ) -> Result<ProofReport, MissionError> {
        let identity = self.proof_identity()?;
        let closure_plan = self
            .closure_plan
            .as_ref()
            .ok_or(MissionError::ClosurePlanMissing)?;
        ProofEvaluator::evaluate(
            &identity,
            closure_plan,
            declarations,
            records,
            current,
            unresolved_attention_ids,
            authority,
        )
        .map_err(|_| MissionError::InvalidProofEvaluation)
    }

    pub(crate) fn evaluate_ready_proof(
        &self,
        declarations: &[CheckDeclaration],
        records: &BTreeMap<String, EvidenceRecord>,
        current: &WorkspaceSnapshot,
        unresolved_attention_ids: &BTreeSet<String>,
        authority: &AuthoritySnapshot,
    ) -> Result<ReadyProof, MissionError> {
        if self.status != MissionStatus::ReviewRequired {
            return Err(MissionError::InvalidTransition {
                from: self.status,
                to: MissionStatus::ReadyToClose,
            });
        }
        let verified = self
            .evaluate_proof(
                declarations,
                records,
                current,
                unresolved_attention_ids,
                authority,
            )?
            .into_verified()
            .map_err(|_| MissionError::InvalidProofEvaluation)?;
        Ok(ReadyProof {
            mission_id: self.definition.id.clone(),
            verified,
        })
    }

    pub(crate) fn evaluate_archive_proof(
        &self,
        declarations: &[CheckDeclaration],
        records: &BTreeMap<String, EvidenceRecord>,
        current: &WorkspaceSnapshot,
        unresolved_attention_ids: &BTreeSet<String>,
        authority: &AuthoritySnapshot,
    ) -> Result<ArchiveProof, MissionError> {
        if self.status != MissionStatus::ReadyToClose {
            return Err(MissionError::InvalidTransition {
                from: self.status,
                to: MissionStatus::Archived,
            });
        }
        let ready = self
            .ready_receipt
            .as_ref()
            .ok_or(MissionError::ReadyReceiptMissing)?;
        let verified = self
            .evaluate_proof(
                declarations,
                records,
                current,
                unresolved_attention_ids,
                authority,
            )?
            .into_verified()
            .map_err(|_| MissionError::InvalidProofEvaluation)?;
        if verified.subject_digest() != ready.subject_digest
            || verified.authority_sequence() <= ready.authority_sequence
            || verified.verified_at_millis() < ready.verified_at_millis
        {
            return Err(MissionError::FreshArchiveProofRequired);
        }
        Ok(ArchiveProof {
            mission_id: self.definition.id.clone(),
            verified,
            ready_seal_digest: ready.seal_digest.clone(),
        })
    }

    fn can_transition(from: MissionStatus, to: MissionStatus) -> bool {
        matches!(
            (from, to),
            (MissionStatus::Preparing, MissionStatus::Active)
                | (MissionStatus::Preparing, MissionStatus::Blocked)
                | (MissionStatus::Preparing, MissionStatus::Failed)
                | (MissionStatus::Active, MissionStatus::ReviewRequired)
                | (MissionStatus::Active, MissionStatus::Blocked)
                | (MissionStatus::Active, MissionStatus::Failed)
                | (MissionStatus::Blocked, MissionStatus::Preparing)
                | (MissionStatus::Blocked, MissionStatus::Active)
                | (MissionStatus::Blocked, MissionStatus::Failed)
                | (MissionStatus::ReviewRequired, MissionStatus::Active)
                | (MissionStatus::ReviewRequired, MissionStatus::Blocked)
                | (MissionStatus::ReviewRequired, MissionStatus::Failed)
                | (MissionStatus::ReadyToClose, MissionStatus::Active)
                | (MissionStatus::ReadyToClose, MissionStatus::Blocked)
        )
    }

    fn record_transition(
        &mut self,
        next: MissionStatus,
        actor: String,
        reason: String,
        at_millis: u64,
    ) -> Result<(), MissionError> {
        if actor.trim().is_empty() {
            return Err(MissionError::EmptyTransitionActor);
        }
        if reason.trim().is_empty() {
            return Err(MissionError::EmptyTransitionReason);
        }
        if self
            .history
            .last()
            .is_some_and(|transition| at_millis < transition.at_millis)
        {
            return Err(MissionError::TransitionTimeWentBackwards);
        }

        let previous = self.status;
        self.status = next;
        self.history.push(MissionTransition {
            from: Some(previous),
            to: next,
            actor,
            reason,
            at_millis,
        });
        Ok(())
    }

    #[must_use]
    pub const fn status(&self) -> MissionStatus {
        self.status
    }

    #[must_use]
    pub const fn run_target(&self) -> Option<&RunTarget> {
        self.run_target.as_ref()
    }

    #[must_use]
    pub fn history(&self) -> &[MissionTransition] {
        &self.history
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "this typed error contract includes staged standalone lifecycle failures"
)]
pub enum MissionError {
    #[error("mission id cannot be empty")]
    EmptyMissionId,
    #[error("mission title cannot be empty")]
    EmptyTitle,
    #[error("repository path must be absolute")]
    RepositoryPathNotAbsolute,
    #[error("mission objective cannot be empty")]
    EmptyObjective,
    #[error("mission needs at least one acceptance criterion")]
    MissingAcceptanceCriteria,
    #[error("mission run id cannot be empty")]
    EmptyRunId,
    #[error("base revision cannot be empty")]
    EmptyBaseRevision,
    #[error("worktree path must be absolute")]
    WorktreePathNotAbsolute,
    #[error("invalid mission transition: {} -> {}", from.as_str(), to.as_str())]
    InvalidTransition {
        from: MissionStatus,
        to: MissionStatus,
    },
    #[error("run target cannot be changed while mission is {}", .0.as_str())]
    RunTargetCannotBeChanged(MissionStatus),
    #[error("mission transition to {} requires a sealed proof command", .0.as_str())]
    SealedTransitionRequired(MissionStatus),
    #[error("verified proof belongs to another mission run or worktree")]
    ProofScopeMismatch,
    #[error("verified proof timestamp is in the future")]
    ProofFromFuture,
    #[error("ready-to-close mission has no proof receipt")]
    ReadyReceiptMissing,
    #[error("proof basis changed after the mission became ready")]
    ProofBasisChanged,
    #[error("archiving requires a newly evaluated proof")]
    FreshArchiveProofRequired,
    #[error("mission proof identity is invalid")]
    InvalidProofIdentity,
    #[error("mission proof evaluation context is invalid")]
    InvalidProofEvaluation,
    #[error("mission closure plan is invalid")]
    InvalidClosurePlan,
    #[error("mission closure plan is missing")]
    ClosurePlanMissing,
    #[error("verified proof does not match the mission closure plan")]
    ProofContractMismatch,
    #[error("transition actor cannot be empty")]
    EmptyTransitionActor,
    #[error("transition reason cannot be empty")]
    EmptyTransitionReason,
    #[error("mission transition time cannot go backwards")]
    TransitionTimeWentBackwards,
    #[error("mission has no run target")]
    MissionHasNoRunTarget,
}
